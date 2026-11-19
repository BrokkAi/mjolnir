# Documented RQL surface

Pin: `bifrost-dev 060087c25162d4846031f1257f3704db360c642d` (HEAD verified).

The table rows below reproduce all step entries under “Steps” in the public CodeQuery/RQL vocabulary reference. Forms and semantics are verbatim summaries from that reference; Rust support is explicit only where noted in the source. “Not established” means the public row makes no Rust claim, not that implementation lacks support. Source lines identify each table.


### Reference table at line 289

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `filter` | `filter` | any row -> same row | `where` (required) | Keep rows for which every typed predicate holds. |
| `project` | `project` | any row -> projected row | `columns` (required) | Publish selected fields, optionally renamed, to later `filter` and `project` steps and to relational policy bindings. |
| `anchor` | `anchor` | `structural_match` -> `structural_match` | `capture` (required) | Emit one row for each distinct node bound to the named positive capture, using that node's source range as the reported location while retaining the structural root for matching and later steps. |
| `identical_captures` | `identical-captures`, `identical_captures` | `structural_match` -> `structural_match` | `left`, `right` (required) | Compare every left capture node with every right capture node. Keep the row if any pair is structurally identical; drop it only if every pair is distinct; otherwise withhold it with `identical_captures_open`. Leaves, including literals, compare by source token: `0x10` and `16` are Distinct spellings, which does not claim different runtime values. Decoded-value comparison is tracked in #10093. |
| `statement_facts` | `statement-facts`, `statement_facts` | `structural_match` -> `structural_match` | `statement_fact_kind` (required) | Retain structural matches for one registered CodeQL statement fact. C/C++-only kinds return no matches for other languages; `nested-loops-with-same-variable` is supplied by both C++ and C#. Supported kinds are `alloca-in-loop`, `complex-block`, `complex-condition`, `empty-block`, `long-switch`, `missing-case-in-switch`, `nested-loops-with-same-variable`, and `use-of-goto`. |
| `cpp_facts` | `cpp-facts`, `cpp_facts` | `structural_match` -> `structural_match` | `kind` (required) | Retain C++ class, special-member, catch, throw, or inheritance matches for one registered CodeQL parity fact. Supported kinds are `assignment-does-not-return-this`, `catch-by-value`, `constructor-delegation`, `constructor-used-as-copy-constructor`, `rule-of-three`, `rule-of-two`, `throwing-pointer`, and `undisciplined-multiple-inheritance`. |

### Reference table at line 304

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `enclosing_decl` | `enclosing-decl` | `structural_match` or `dispatch_target` -> `declaration` | none | The smallest real indexed declaration that contains the match or dispatch call site. |
| `file_of` | `file-of` | most row domains -> `file` | none | The workspace file that holds the row. Accepted inputs are listed below this table. |
| `imports_of` | `imports-of` | `file` -> `file` | none | Project files that the input file's resolved imports point to. For Rust, this includes the file that defines an item reached through `pub use` re-exports. |
| `importers_of` | `importers-of` | `file` -> `file` | none | Project files whose resolved imports point to the input file. For Rust, a file that reaches an item in the input file through `pub use` re-exports is an importer. |
| `python_import_site` | `python-import-site` | `structural_match` -> `structural_match` | none | Add parser-derived Python import leaves, local target resolution, scope, duplicate, exact `except ImportError` handler context, self-import, and `__all__` facts. |
| `python_import_cycles` | `python-import-cycles` | `structural_match` -> `structural_match` | none | Mark Python import sites with a confirmed cycle in the persisted workspace file graph; unresolved targets and absent paths remain incomplete. |
| `python_commented_out_code` | `python-commented-out-code` | `structural_match` -> `python_rule_fact` | none | Return one source-spanned row for each Python commented-out-code block recognized from tree-sitter comment nodes. Rows retain exact comment lines and text; CodeQL-style example-heavy, documentation-like, filler, path, URL, doctest, and end-of-line exclusions apply. |

### Reference table at line 365

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `supertypes` | `supertypes` | `declaration` -> `declaration` | `depth` or `transitive` | Indexed supertypes: direct by default, up to `depth` hops, or the full closure. A semantic-model hierarchy that names a type no model declares stops there and reports the answer incomplete. |
| `subtypes` | `subtypes` | `declaration` -> `declaration` | `depth` or `transitive` | Indexed subtypes: direct by default, up to `depth` hops, or the full closure. A semantic-model hierarchy that names a type no model declares stops there and reports the answer incomplete. |
| `members` | `members` | `declaration` -> `declaration` | none | Direct indexed members of a type. |
| `owner` | `owner` | `declaration` -> `declaration` | none | The exact indexed type that declares a member. |
| `member_family` | `member-family`, `member_family` | `declaration` -> `member_family` | none | One method-family outcome row per member: the family ID when proven, otherwise a typed reason and unread ancestor names. Coverage is reported for the whole family and for each direction. |
| `family_edges` | `family-edges`, `family_edges` | `declaration` -> `member_family_edge` | none | Proven `overrides` and `implements` edges of a member, plus their inverses `overridden_by` and `implemented_by`. |
| `python_override_signatures` | `python-override-signatures`, `python_override_signatures` | `declaration` -> `python_hierarchy_fact` | none | C3-selected Python override paired with each source and target callable-signature entry, including stable signature IDs, arity, and coverage. |

### Reference table at line 377

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `references_of` | `references-of` | `declaration` -> `reference_site` | `reference_kinds`, `enclosing`, `proof`, `surface` | Resolved source sites that refer to the declaration. |
| `used_by` | `used-by` | `declaration` -> `declaration` | `reference_kinds`, `proof`, `surface` | Declarations that contain references to the input. |
| `uses` | `uses` | `declaration` -> `declaration` | `reference_kinds`, `proof`, `surface` | Declarations that the input refers to. |
| `callers` | `callers` | `declaration` -> `declaration` | `depth`, `proof`, `completeness` | Callers through resolved incoming call edges, direct or up to `depth` hops. |
| `callees` | `callees` | `declaration` -> `declaration` | `depth`, `proof`, `completeness` (only `exhaustive`) | Callees through resolved outgoing call edges, direct or up to `depth` hops. |
| `call_sites_to` | `call-sites-to`, `call_sites_to` | `declaration` -> `call_site` | `proof` | Call sites whose resolved callee is the declaration. |
| `call_sites_from` | `call-sites-from`, `call_sites_from` | `declaration` -> `call_site` | `proof` | Call sites written inside the declaration. |
| `call_input` | `call-input`, `call_input` | `call_site` -> `expression_site` | exactly one of `receiver`, `parameter_index`, `parameter_name` | The receiver expression, or the argument bound to one formal parameter. |

### Reference table at line 390

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `edges_of` | `edges-of`, `edges_of` | `declaration` -> `reference_edge` | `reference_kinds`, `enclosing`, `proof`, `surface`, `usage`, `relation`, `site_class` | Inverse edges: every usage site the usage index lists for the declaration. |
| `edges_from` | `edges-from`, `edges_from` | `occurrence` -> `reference_edge` | same seven options | Forward edges: the targets the resolver chose for that exact token. |
| `edge_target` | `edge-target`, `edge_target` | `reference_edge` -> `declaration` | none | The exact indexed target declaration of an edge. |

### Reference table at line 398

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `occurrences_of` | `occurrences-of`, `occurrences_of` | `declaration` -> `occurrence` | `class`, `role`, `namespace` | The declaration's name occurrence plus every reference occurrence that resolves to it. |
| `occurrences_in` | `occurrences-in`, `occurrences_in` | `structural_match`, `lexical_scope`, or `file` -> `occurrence` | `class`, `role`, `namespace` | Classified identifier occurrences inside the match or file, or whose exact innermost scope is the supplied lexical scope. |
| `occurrence_target` | `occurrence-target`, `occurrence_target` | `occurrence` -> `declaration` | none | The declarations that a reference occurrence resolved to. |
| `scope_of` | `scope-of`, `scope_of` | `binding`, `occurrence`, or `structural_match` -> `lexical_scope` | none | The innermost lexical scope that owns the input. |
| `scope_ancestors` | `scope-ancestors`, `scope_ancestors` | `lexical_scope` -> `lexical_scope` | none | The enclosing scopes, innermost first, without the scope itself. |
| `bindings_in` | `bindings-in`, `bindings_in` | `lexical_scope` or `structural_match` -> `binding` | `kind`, `name`, `hoisting` | Bindings declared in the scope, or in the scopes inside the match. |
| `binding_of` | `binding-of`, `binding_of` | `occurrence` -> `binding` | `include_shadowed` | The binding in effect at a reference's exact position; a binder token preceding its own activation identifies its introduced binding instead. |
| `binding_occurrence` | `binding-occurrence`, `binding_occurrence` | `binding` -> `occurrence` | none | The binder occurrence of the binding's declaring token. |
| `binding_uses` | `binding-uses`, `binding_uses` | `binding` -> `occurrence` | none | Reference occurrences whose exact lexical `binding_of` result is the input binding, joined from per-file occurrence and binding facts at query time. |
| `python_variable_of` | `python-variable-of`, `python_variable_of` | `binding` or `occurrence` -> `python_rule_fact` | none | Map one Python binder or identifier to the query-local variable grouped by its resolved lexical scope and name; `global` and `nonlocal` declarations redirect to their owning scope. |
| `python_variable_definitions` | `python-variable-definitions`, `python_variable_definitions` | `python_rule_fact` -> `python_rule_fact` | none | Return the source-ordered definitions of a lexical-variable row, with kind, first-definition status, and reachability. |
| `python_variable_uses` | `python-variable-uses`, `python_variable_uses` | `python_rule_fact` -> `python_rule_fact` | none | Return the reads and writes of a lexical-variable row, including closure reads from nested scopes. |
| `python_variable_peers` | `python-variable-peers`, `python_variable_peers` | `python_rule_fact` -> `python_rule_fact` | none | Return same-name lexical variables from other scopes in the same file, with module-definition exclusion facts. |
| `python_variable_nested_loops` | `python-variable-nested-loops`, `python_variable_nested_loops` | `python_rule_fact` -> `python_rule_fact` | none | Return nested `for` loops targeting one lexical variable, with outer-use and post-inner-loop reuse facts. Reuse requires a later read in the same statement block with no intervening definition; an unconditional write in the inner loop body prevents the reuse fact. |
| `identical_operands` | `identical-operands`, `identical_operands` | `structural_match` -> `structural_match` | none | Python pairwise comparisons whose bare names share an exact binding, or whose static attribute paths share member names and root binding; unresolved supported bindings make the query incomplete. |
| `python_missing_self` | `python-missing-self`, `python_missing_self` | `structural_match` -> `structural_match` | none | Python method comparisons of the same bare name without a local binding or explicit scope directive, when a same-class `self.<name>` access resolves `self` to a method parameter. |
| `candidates_of` | `candidates-of`, `candidates_of` | `occurrence` -> `resolution_candidate` | `tier`, `outcome`, `boundary` | Every candidate the resolver considered for a reference occurrence. |
| `candidate_hierarchy` | `candidate-hierarchy`, `candidate_hierarchy` | `occurrence` -> `candidate_hop` | none | The hierarchy hops through which each traced member candidate was found. |
| `candidate_target` | `candidate-target`, `candidate_target` | `resolution_candidate` -> `declaration` | none | The workspace declaration of a unit-backed candidate. Other candidates produce no row. |
| `member_selection` | `member-selection`, `member_selection` | `occurrence` -> `member_selection` | none | One member-selection summary row per reference occurrence, from the resolver's candidate trace. |
| `callable_applicability` | `callable-applicability`, `callable_applicability` | `occurrence` -> `callable_applicability` | none | One row per candidate callable the resolver considered, with its applicability verdict (`applicable`, `less_specific`, `inapplicable`, or `unknown`), its rejection reason, and, where the resolver records them, the invocation phase that decided it and the first failing argument with the conversion it would need. |
| `overload_selection` | `overload-selection`, `overload_selection` | `occurrence` -> `overload_selection` | none | One overload-selection summary per reference occurrence: `resolved_unique`, `ambiguous`, `unresolved`, or `unknown_shape`. |

### Reference table at line 425

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `segments_of` | `segments-of`, `segments_of` | `qualified_path` -> `path_segment` | `resolved` | The ordered segments of a path, with decoded text and spelled generic arity. With `resolved`, each segment also carries its own resolution. |
| `segment_target` | `segment-target`, `segment_target` | `path_segment` -> `declaration` | none | The workspace declarations that one segment resolves to. |

### Reference table at line 432

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `receiver_targets` | `receiver-targets`, `receiver_targets` | `structural_match`, `reference_site`, `call_site`, `expression_site`, or `occurrence` -> `receiver_analysis` | `capture` | Analyze the receiver value of a call or member site, within bounds. |
| `points_to` | `points-to`, `points_to` | `structural_match`, `reference_site`, `expression_site`, or `occurrence` -> `receiver_analysis` | `capture` | Analyze where an expression's value comes from, within bounds. |
| `member_targets` | `member-targets`, `member_targets` | `structural_match`, `reference_site`, or `occurrence` -> `member_target_analysis` | `capture`, `member_form`, `trivial_property` | Resolve the exact static member, with the receiver owner and model provenance. With `member_form` or `trivial_property`, keep only the rows whose single exact target satisfies every option; a row whose answer is undecided is withheld and reported as `member_form_undecided`. |
| `receiver_outcome` | `receiver-outcome`, `receiver_outcome` | `structural_match`, `reference_site`, `call_site`, `expression_site`, `occurrence`, `receiver_analysis`, or `member_target_analysis` -> `receiver_outcome` | none | Exactly one terminal outcome row per analyzed site. |
| `receiver_evidence` | `receiver-evidence`, `receiver_evidence` | `structural_match`, `reference_site`, `call_site`, `expression_site`, `occurrence`, or `receiver_analysis` -> `receiver_evidence` | none | Zero or more parent-linked evidence rows per analyzed site. |
| `dispatch_outcome` | `dispatch-outcome`, `dispatch_outcome` | `structural_match`, `call_site`, `reference_site`, or `occurrence` -> `dispatch_outcome` | none | Exactly one bounded-dispatch outcome row per site, with coverage and target count. |
| `dispatch_targets` | `dispatch-targets`, `dispatch_targets` | `structural_match`, `call_site`, `reference_site`, or `occurrence` -> `dispatch_target` | none | One row per retained dispatch candidate, and per boundary arm that names a target. |

### Reference table at line 446

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `call_shape` | `call-shape`, `call_shape` | `structural_match`, `call_site`, or `occurrence` -> `call_shape` | none | Exactly one structured call-shape row per call site, with the decoded callee token and written argument count. |
| `call_results` | `call-results`, `call_results` | `call_shape` -> `call_result` | none | The ordered normal result values of the semantic call. |
| `call_argument_groups` | `call-argument-groups`, `call_argument_groups` | `call_shape` -> `call_argument_group` | none | The ordered argument-list groups of a call. |
| `call_arguments` | `call-arguments`, `call_arguments` | `call_argument_group` -> `call_argument` | none | The ordered arguments of one argument-list group. |
| `call_bindings` | `call-bindings`, `call_bindings` | `call_shape` -> `call_binding` | none | Actual-to-formal binding rows, plus rows for the receiver, implicit arguments, defaulted formals, and Python required formals definitely left unbound. Unknown Python keywords have a distinct reason. At least one row per call. |
| `resolved_call` | `resolved-call`, `resolved_call` | `call_binding` -> `call_binding` | `resolves_to` and `call_proof` (both required), `receiver_type` | Keep binding rows whose call resolves to one callable identity with `exact` or `declared` proof. |
| `resolved_value` | `resolved-value`, `resolved_value` | `call_binding` or `structural_match` -> same row | `resolves_to` and exact `call_proof` (both required) | Keep a bound call actual, Python decorator expression, Python structured value node, or C# structured value node only when the bounded resolver proves the requested exact declaration identity. Unresolved, ambiguous, shadowed, unsupported, or budget-limited values make the query incomplete. |
| `call_argument` | `call-argument`, `call_argument` | `call_binding` -> `call_binding` | exactly one of `formal_name`, `formal_index`, `formal_annotation_type` | Keep the source argument bound to one formal parameter, with exact mapping and exhaustive coverage. |
| `cpp_bad_strncpy_size` | `cpp-bad-strncpy-size`, `cpp_bad_strncpy_size` | `call_binding` -> `call_binding` | none | Keep a call-binding row when structured fixed-array extents prove that a `strncpy` source is larger than its destination and the policy has already selected the exact declared library identity. |
| `cpp_network_to_host_function_as_array_bound` | `cpp-network-to-host-function-as-array-bound`, `cpp_network_to_host_function_as_array_bound` | `call_binding` -> `call_binding` | none | Keep a call-binding row when structured syntax proves a direct `ntohl` result is passed as the `memcpy` bound after exact declared call selection. |
| `callable_signature` | `callable-signature`, `callable_signature` | `declaration` -> `callable_signature` | none | One row per persisted signature of a declaration; an overload set gives one row per overload. Python special methods additionally carry their query-time protocol arity verdict and placeholder status. |
| `signature_parameters` | `signature-parameters`, `signature_parameters` | `callable_signature` -> `signature_parameter` | none | The ordered declared parameters of a signature. |
| `decorator_bindings` | `decorator-bindings`, `decorator_bindings` | `structural_match` -> `decorated_parameter` | `module`, `imported_name`, `annotation_type` | One row for each decorator applied to a matched parameter. |

### Reference table at line 464

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `cpp_expressions` | `cpp-expressions`, `cpp_expressions` | `structural_match` -> `cpp_expression` | none | Project occurrence-keyed C++ literal, array-parameter, function-parameter, bit-field, enum, function-signature, loop-variable, and `sizeof` facts from the selected AST range. Scalar values that the source does not prove remain null, and `completeness` is `open` with a reason when the row is partial. |

### Reference table at line 481

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `call_effects` | `call-effects`, `call_effects` | `call_shape` -> `call_effect` | none | One row per dispatch arm and declared effect from an active semantic pack. At least one row per call. |
| `procedure_effects` | `procedure-effects`, `procedure_effects` | `declaration` -> `procedure_effect` | none | One row per procedure and effect over its reachable call graph, direct or transitive, with coverage. |
| `result_contract_calls` | `result-contract-calls`, `result_contract_calls` | `call_shape` -> `call_shape` | none | Keep calls whose every dispatch arm selects a reviewed result contract. |
| `call_result_contracts` | `call-result-contracts`, `call_result_contracts` | `call_shape` -> `call_result_contract` | none | Reviewed validity contracts for a call's normal results. At least one row per call. |
| `call_result_obligations` | `call-result-obligations`, `call_result_obligations` | `call_shape` -> `call_result_obligation` | none | Java result-use and return-use obligations from the selected JDK model. |
| `result_contract_uses` | `result-contract-uses`, `result_contract_uses` | `call_result_contract` -> `call_result_contract` | none | Add aggregate counts of operations on the protected result to each contract row. |
| `result_subject_uses` | `result-subject-uses`, `result_subject_uses` | `call_result` -> `result_subject_use` | none | Procedure-local uses of a call result as a receiver. |
| `result_contract_operation_uses` | `result-contract-operation-uses`, `result_contract_operation_uses` | `call_result_contract` -> `result_contract_use` | none | One row per structured operation on a protected result, with a guard verdict. |
| `result_contract_failure_uses` | `result-contract-failure-uses`, `result_contract_failure_uses` | `call_result_contract` -> `result_contract_failure_use` | `provenance`, `consumer` | Values returned or passed to calls inside the failure arm of a conditional result contract. |

### Reference table at line 495

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `procedure_of` | `procedure-of`, `procedure_of` | `structural_match` or `declaration` -> `procedure` | none | The smallest source-backed executable procedure that contains the input. |
| `program_point_of` | `program-point-of`, `program_point_of` | `structural_match` -> `program_point` | none | The unique point whose source mapping retains the exact structural match identity. For `await`, this is its `AsyncSuspend`. Missing or ambiguous points, incomplete producer capability coverage, exhausted traversal budgets and cancellation report incomplete. |
| `program_point_by_id` | `program-point-by-id`, `program_point_by_id` | `procedure` -> `program_point` | required `id` | Look up an observed content-scoped public point ID in the supplied current procedure. This selects a physical point and does not claim exhaustive AST/read discovery. Missing, stale, wrong-procedure, ambiguous or incomplete selection reports incomplete. IDs do not certify semantic-overlay freshness; downstream proofs acquire current evidence. |
| `selected_property_relations_of` | `selected-property-relations-of`, `selected_property_relations_of` | `program_point` -> `flow_relation` | none | Reaching relations for the unique property read at that exact point. Completeness covers only the selected durable receiver/member and its dependencies and endpoints; unknown values stay unknown. Missing or ambiguous reads and open alias, runtime, control, budget or cancellation evidence report incomplete. |
| `field_writes_of` | `field-writes-of`, `field_writes_of` | `declaration` -> `field_write` | none | Each write to a field declaration that the semantic IR models as a field store: an assignment, a compound assignment, a struct-literal initializer or its shorthand. A write is located by a proven usage of the field in the access, so an equally named field of another struct is never returned. Complete only when the usage scan is complete and proven, every procedure around a usage lowered with field-memory support, and no relevant gap, address-of, unsupported pointer or macro fact, or unresolved store through a pointer could hide a write; otherwise the answer is incomplete. A declaration that is not a field returns no rows and an advisory `no_enclosing_procedure` diagnostic. |
| `field_reads_of` | `field-reads-of`, `field_reads_of` | `declaration` -> `field_read` | none | Each read of a field declaration that the semantic IR models as a field load, located and completed as `field_writes_of` is, with read-side gaps and unresolved loads through a pointer. |
| `cfg_entry` | `cfg-entry`, `cfg_entry` | `procedure` -> `program_point` | none | The entry point of the procedure's control-flow graph. |
| `cfg_exits` | `cfg-exits`, `cfg_exits` | `procedure` -> `program_point` | none | The normal and exceptional exit points of the procedure. |
| `cfg_successor_edges` | `cfg-successor-edges`, `cfg_successor_edges` | `program_point` -> `control_edge` | none | Outgoing control edges, one hop. |
| `cfg_predecessor_edges` | `cfg-predecessor-edges`, `cfg_predecessor_edges` | `program_point` -> `control_edge` | none | Incoming control edges, one hop. |
| `cfg_edge_source` | `cfg-edge-source`, `cfg_edge_source` | `control_edge` -> `program_point` | none | The source point of an edge. |
| `cfg_edge_target` | `cfg-edge-target`, `cfg_edge_target` | `control_edge` -> `program_point` | none | The target point of an edge. |
| `control_relations` | `control-relations`, `control_relations` | `procedure` -> `control_relation` | `control_relation`, `exit_partition` | Dominance, postdominance, control dependence, entry reachability, and loop membership between program points. |
| `guards_of` | `guards-of`, `guards_of` | `procedure` -> `guard` | none | The normalized branch conditions the adapter recorded, with their true and false successor edges. |
| `branch_relations` | `branch-relations`, `branch_relations` | `structural_match` -> `branch_relation` | `branch_relation` | Compare the ordered arms of one `if` match: identical bodies, repeated or contradictory conditions, and similar relations. |
| `cpp_expressions` | `cpp-expressions`, `cpp_expressions` | `structural_match` -> `cpp_expression` | none | Project typed, occurrence-keyed C++ expression facts from the selected source range. |
| `cpp_inconsistent_loop_direction` | `cpp-inconsistent-loop-direction`, `cpp_inconsistent_loop_direction` | `structural_match` -> `structural_match` | none | Keep a C/C++ `for` match when its structured condition direction and update direction disagree for the same loop variable. |
| `cpp_unsafe_array_for_days_of_the_year` | `cpp-leap-year-unsafe-array-for-days-of-the-year`, `cpp_unsafe_array_for_days_of_the_year` | `structural_match` -> `structural_match` | none | Keep a C/C++ numeric-literal match when the structured literal `365` is a fixed array or vector-constructor extent. |
| `cpp_provenance` | `cpp-provenance`, `cpp_provenance` | `structural_match` -> `structural_match` | `cpp_provenance_rule` | Filter a C/C++ return, assignment, or call anchor through one direct procedure-local provenance rule. The source-backed terminal anchor is preserved; unresolved aliases and call identities remain incomplete. |
| `loop_relations` | `loop-relations`, `loop_relations` | `procedure` -> `loop_relation` | none | For a Java loop, prove whether its body can reach its own repeat edge before it exits. |
| `statement_reachability` | `statement-reachability`, `statement_reachability` | `procedure` -> `statement_reachability` | none | For Java statements, report whether bounded entry reachability reaches any of the statement's control-flow entries. |
| `switch_coverage` | `switch-coverage`, `switch_coverage` | `procedure` -> `switch_coverage` | none | One coverage verdict per switch: exhaustive, non-exhaustive, or unknown. |
| `failure_handler_state` | `failure-handler-state`, `failure_handler_state` | `structural_match` -> `failure_handler_state` | none | Classify a catch, except, rescue, or native Go/Rust failure candidate body as empty, nonempty, or unknown. |

### Reference table at line 531

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `state_events_of` | `state-events-of`, `state_events_of` | `procedure` or `declaration` -> `state_event` | `event_class`, `subject` | Establish, kill, and read events of bindings and properties, placed on the control-flow graph. |
| `flow_relations_of` | `flow-relations-of`, `flow_relations_of` | `state_event` or `procedure` -> `flow_relation` | `flow_relation`, `certainty` | Reaching, dominance, and same-evaluation relations between state events. |
| `flow_source` | `flow-source`, `flow_source` | `flow_relation` -> `state_event` | none | The establish or kill end of a relation. |
| `flow_target` | `flow-target`, `flow_target` | `flow_relation` -> `state_event` | none | The read end of a relation. |
| `value_flow` | `value-flow`, `value_flow` | `procedure` -> `flow_endpoint` | `plan_ref` (required) | Run one host-registered value-flow plan for the exact procedure. |
| `taint` | `taint` | `procedure` -> `taint_finding` | `taint_ref` (required) | Project the findings of one host-registered production taint result for the exact procedure. |
| `typestate` | `typestate` | `procedure` -> `typestate_finding` | `protocol_ref` (required) | Run one host-registered typestate protocol for the exact procedure. |
| `witness` | `witness` | `typestate_finding`, `flow_endpoint`, or `absent_member_finding` -> `typestate_witness`, `flow_witness`, or `absent_member_witness` | `max_steps`, `max_bytes` | Project the retained evidence of each finding without running the analysis again. |
| `class_set` | `class-set`, `class_set` | `procedure` -> `class_set_row` | none | For each member access reachable from the procedure, the classes its receiver may hold. |
| `python_iterator_return_protocol` | `python-iterator-return-protocol`, `python_iterator_return_protocol` | `procedure` -> `python_rule_fact` | none | For each value returned by a Python `__iter__`, classify its bounded runtime classes by whether they declare inherited `__iter__` and `__next__`; expose exact direct receiver returns as `returns_receiver`; unresolved or incomplete class sets stay open. |
| `absent_member` | `absent-member`, `absent_member` | `procedure` -> `absent_member_finding` | `finding_proof` | Member accesses where a class reaches the site and lacks the member; Proven requires a fully known receiver set, while Conditional admits only the documented receiver, value-flow, and write remainders. |
| `nilness_operations` | `nilness-operations`, `nilness_operations` | `procedure` -> `nilness_operation` | none | Pointer operations with their procedure-local nilness fact. Go only; other languages yield no rows and an incomplete `unsupported_structural_feature` diagnostic. |
| `assignment_relations` | `assignment-relations`, `assignment_relations` | `procedure` -> `assignment_relation` | `assignment_relation` | Self-assignments, local self-initializers, failed swaps, and overwritten-unread assignments. The bounded `self_initializer` slice currently supports C/C++ local declarations; other relation coverage is language-specific and reports `effect_derivation_incomplete` when unsupported. |
| `detached_task_transfers` | `detached-task-transfers`, `detached_task_transfers` | `procedure` -> `detached_task_transfer` | none | Values copied into calls that run detached in a different task. |

### Reference table at line 550

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `python_builtin_identity` | `python-builtin-identity`, `python_builtin_identity` | `occurrence` -> `python_rule_fact` | none | Resolve an unqualified Python builtin reference using its exact lexical binding. A local or module shadow is partial; missing or wildcard-import evidence stays open. For a proven direct call callee, the result also has `call_callee=true`. |
| `python_uninitialized_variable` | `python-uninitialized-variable`, `python_uninitialized_variable` | `occurrence` -> `python_rule_fact` | none | Join an exact Python value reference to its lexical binding and procedure-local CFG read. Emit a finding only for a complete may-be-uninitialized answer; globals, class scopes, builtins, closures and open flow identities stay incomplete. |
| `python_assertion_context` | `python-assertion-context`, `python_assertion_context` | `structural_match` -> `python_rule_fact` | none | Add tri-state `test_scope` and `terminal_elif_else` fields to the shared Python rule-fact row for one assertion. Unresolved unittest inheritance remains `unknown`. |
| `tuple_return_assignment` | `tuple-return-assignment`, `tuple_return_assignment` | `structural_match` -> `python_rule_fact` | none | Resolve a direct local call assigned to a tuple or list and compare its exact distinct return arities with the direct assignment target count. |
| `python_implicit_string_concatenation` | `python-implicit-string-concatenation`, `python_implicit_string_concatenation` | `structural_match` -> `python_rule_fact` | none | Emit one anchored row for every literal part implicitly concatenated inside a list element. |
| `python_format_issues` | `python-format-issues`, `python_format_issues` | `structural_match` -> `python_rule_fact` | none | Inspect exact Python `str.format` calls and `%` binary expressions for five brace-format and three percent-format issues. Format values and supported single-initializer aliases are resolved structurally; unsupported or ambiguous shapes remain partial. |
| `python_url_construction` | `python-url-construction`, `python_url_construction` | `structural_match` -> `python_rule_fact` | none | Project typed per-slot full or partial URL-control facts for Python `+`, f-strings, `%` formatting, and literal `str.format` construction. Unknown format strings and operands remain incomplete. |
| `python_raise_not_implemented` | `python-raise-not-implemented`, `python_raise_not_implemented` | `structural_match` -> `python_rule_fact` | none | Identify direct, call-form, or one-step aliased raises of the builtin `NotImplemented` singleton. A shadow is partial, unresolved identity is open, and `NotImplementedError` is distinct and excluded. |
| `jsx_attribute_value` | `jsx-attribute-value`, `jsx_attribute_value` | `structural_match` -> `jsx_attribute_value` | `identity`, `element_name`, `property_name` | The exact expression of a JSX attribute value, with the element's intrinsic, component, or unknown identity. |
| `keyed_read_value` | `keyed-read-value`, `keyed_read_value` | `structural_match` -> `keyed_read_value` | `runtime`, `global`, `container` (all required); one of `property`, `index`, `key_kind`; `index_min`, `index_max`, `source_origin` | A keyed read of a runtime input container, such as `process.env.HOME` or `sys.argv[1]`, resolved to its value. |
| `field_write_value` | `field-write-value`, `field_write_value` | `member_target_analysis` -> `field_write_value` | `receiver_identity_id`, `member_target_id` | The right-hand expression of a simple field assignment whose member and receiver `member_targets` proved. |

### Reference table at line 569

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `target_of` | `target-of`, `target_of` | `file` -> `build_target` | none | The build target that the workspace's build files assign the file to. |
| `source_set_of` | `source-set-of`, `source_set_of` | `file` -> `source_set` | none | The declared source set that contains the file, with the build file that defines it. |
| `topology_edges_of` | `topology-edges-of`, `topology_edges_of` | `build_target` -> `topology_edge` | none | Dependencies that a build target declares on other targets in the workspace. |

### Reference table at line 590

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `build_configuration_of` | `build-configuration-of`, `build_configuration_of` | `file` -> `build_configuration` | none | The build configuration the file is analyzed under, whether that configuration compiles the file, and the target operating systems and architectures the file's own constraint admits. |

### Reference table at line 635

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `generates` | `generates` | `generation_site` -> `declaration_state` | none | The state rows of the declarations a generation site creates. |
| `generated_by` | `generated-by`, `generated_by` | `declaration` or `declaration_state` -> `generation_site` | none | The generation site that created a generated declaration. |
| `declaration_state_of` | `declaration-state-of`, `declaration_state_of` | `declaration` -> `declaration_state` | `origin`, `declaration_only`, `config_gated` | The state row of a declaration: origin, declaration-only flag, and configuration gate. |
| `implementation_of` | `implementation-of`, `implementation_of` | `declaration_state` or `declaration` -> `declaration` | none | The runnable implementation that a declaration-only signature links to. |
| `stubs_of` | `stubs-of`, `stubs_of` | `declaration` -> `declaration_state` | none | The declaration-only stubs whose implementation link resolves to the declaration. |
| `export_target` | `export-target`, `export_target` | `export` -> `declaration` | none | The declaration an export row created, where the analyzer models one. |

### Reference table at line 646

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `rewrite_paths_of` | `rewrite-paths-of`, `rewrite_paths_of` | `file` -> `rewrite_path` | `domain`, `rewrite_outcome` | The bounded rewrite chases the file takes part in, with ordered steps, the declared bound, and the outcome. |

## Grammar comparison and composition

At this pin, parser wrapper/source vocabulary is registered in `crates/bifrost-rql/src/query/schema.rs` (`query_step_ops!`, beginning near line 844) and parsing is validated in `crates/bifrost-rql/src/query/source/rql.rs`. I found no step op that parses but is omitted from the public reference's step tables: the reference explicitly claims its tables enumerate every accepted step, and the MCP `steps` parameter is generated from the same `QueryStepOp` registry. This is a comparison of step operations, not all RQL forms. Grammar-only/non-step forms include root controls and combinators (for example `explain`, `profile`, `union`, `intersect`, `except`, `correlate`, `require-both`, `call-reachability`); their forms are described in the RQL guide. Predicate forms are likewise pattern syntax, not pipeline steps. No undocumented step surfaced in the comparison.

A query starts from a structural pattern (normalized kind or name/text predicate anchor), or from a dedicated source form such as `occurrences`, `scopes`, `bindings`, `paths`, `generation-sites`, `exports`, and configuration/dependency evidence. Sources emit their own row domain; they are not transformations over an implicit whole-workspace row set. A step wraps the query producing its input, so inner forms run first; the resulting ordered pipeline must have compatible adjacent input/output row domains. Set branches combine query results; typed continuations require compatible terminal domains, and a common suffix can follow the set. `filter` retains rows meeting every typed predicate; `project` publishes selected/renamed public fields and thereafter only filter/project can follow. Limits include query/pattern/source budgets and step-specific bounds (depth, witness caps, etc.); they bound evidence and can make results incomplete, not prove absence. Root `explain` lowers/selects without scanning; `profile` executes and reports measurements.

## Candidate missing-step facts (pin-specific)

- **Value flow:** documented, not grammar-only or absent. `value-flow` takes a `procedure` plus required `:plan-ref "namespace:name"` and emits endpoints. It runs one immutable plan registered by the host; the RQL author selects a registered plan, not an arbitrary procedure-level flow algorithm. The registry implementation and plan catalog are outside the RQL parser/reference; the reference example names `embedding:request-to-sink`, and other names depend on host registrations. Thus it cannot be assumed that every workspace/procedure has endpoints: only procedures analyzed under the selected registered plan can return them. The public syntax supports Rust only to the extent the host plan/provider supports Rust; the reference does not promise Rust support for arbitrary plans. The registry contract is explicit in `crates/bifrost-rql/src/query/schema.rs` near lines 860, 3810 and 590-640. The reference's value-flow section is near line 535.
- **Field writes and their values:** both documented. `field-writes-of` enumerates writes to an indexed field declaration; it has Rust field-store support subject to semantic IR lowering and explicit completeness conditions. `field-write-value` projects the RHS for a proven receiver/member target; it is documented, but its public reference does not state Rust language support, so support is not established there. Reference lines near 472 and 559 (verify these line numbers against the relevant table in this pinned file when using the claim).
- **Argument values at call sites:** documented as `call-input`, from a `call_site` to the receiver expression or argument bound to a formal parameter. Rust argument extraction follows whatever call/binding facts the Rust adapter emits; the RQL reference does not claim general Rust coverage, so this is not evidence of universal Rust support. See `docs/src/content/docs/reference/code-query.md` around the call step table and `docs/src/content/docs/rune-query-language.md` around line 373.
- **Callers/callees:** documented, neither grammar-only nor absent. `callers` and `callees` traverse resolved incoming/outgoing call edges, optionally by depth, with proof/completeness controls. Rust works where indexed resolved call edges exist; the reference does not certify exhaustive Rust call-graph coverage, so callers/callees results retain their proof/completeness qualifications. Reference lines 382-383; Rust-specific coverage caveats are in the RQL guide near lines 283-287.

### Documentation boundary

The MCP tool's top-level discovery description is deliberately generic; its generated `steps` parameter carries every step's typed signature and prose description. The reference page is therefore the compact source for this census, while the generated schema/help is also user/agent-facing documentation. The grammar confirms accepted syntax but does not itself make a step documented. The ten-minute evaluation fixture demonstrates a small structural query and policy selector; it does not define additional step semantics.
