# Documented RQL surface

Pin: `bifrost-dev 060087c25162d4846031f1257f3704db360c642d` (HEAD verified).

The first tables cover the public step reference at `docs/src/content/docs/reference/code-query.md`. The additional tables cite the public guide, tutorials, and generated MCP help. Rust support is stated where the sources describe it.


Required options and exactly-one selectors in these tables use RQL keyword
spellings. A slash between keywords marks aliases of one option.

### Reference table at line 289

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `filter` | `filter` | any row -> same row | `:where` (required) | Keep rows for which every typed predicate holds. |
| `project` | `project` | any row -> projected row | `:columns` (required) | Publish selected fields, optionally renamed, to later `filter` and `project` steps and to relational policy bindings. |
| `anchor` | `anchor` | `structural_match` -> `structural_match` | `:capture` (required) | Emit one row for each distinct node bound to the named positive capture, using that node's source range as the reported location while retaining the structural root for matching and later steps. |
| `identical_captures` | `identical-captures`, `identical_captures` | `structural_match` -> `structural_match` | `:left`, `:right` (both required) | Compare every left capture node with every right capture node. Keep the row if any pair is structurally identical; drop it only if every pair is distinct; otherwise withhold it with `identical_captures_open`. Leaves, including literals, compare by source token: `0x10` and `16` are Distinct spellings, which does not claim different runtime values. Decoded-value comparison is tracked in #10093. |
| `statement_facts` | `statement-facts`, `statement_facts` | `structural_match` -> `structural_match` | `:kind` (required) | Retain structural matches for one registered CodeQL statement fact. C/C++-only kinds return no matches for other languages; `nested-loops-with-same-variable` is supplied by both C++ and C#. Supported kinds are `alloca-in-loop`, `complex-block`, `complex-condition`, `empty-block`, `long-switch`, `missing-case-in-switch`, `nested-loops-with-same-variable`, and `use-of-goto`. |
| `cpp_facts` | `cpp-facts`, `cpp_facts` | `structural_match` -> `structural_match` | `:kind` (required) | Retain C++ class, special-member, catch, throw, or inheritance matches for one registered CodeQL parity fact. Supported kinds are `assignment-does-not-return-this`, `catch-by-value`, `constructor-delegation`, `constructor-used-as-copy-constructor`, `rule-of-three`, `rule-of-two`, `throwing-pointer`, and `undisciplined-multiple-inheritance`. |

### Reference table at line 304

| JSON `op` | RQL form | Input -> output | Options | Meaning |
| --- | --- | --- | --- | --- |
| `enclosing_decl` | `enclosing-decl` | `structural_match` or `dispatch_target` -> `declaration` | none | The smallest real indexed declaration that contains the match or dispatch call site. |
| `file_of` | `file-of` | `structural_match`, `declaration`, `procedure`, `program_point`, `control_edge`, `typestate_finding`, `typestate_witness`, `flow_endpoint`, `flow_witness`, `class_set_row`, `absent_member_finding`, `absent_member_witness`, `taint_finding`, `reference_site`, `call_site`, `expression_site`, `jsx_attribute_value`, `receiver_analysis`, `member_target_analysis`, `keyed_read_value`, `receiver_outcome`, `receiver_evidence`, `field_write_value`, `field_write`, `field_read`, `call_shape`, `call_result`, `result_subject_use`, `call_argument_group`, `call_argument`, `call_binding`, `call_effect`, `call_result_contract`, `call_result_obligation`, `result_contract_use`, `result_contract_failure_use`, `procedure_effect`, `callable_signature`, `literal_occurrence`, `callable_metrics`, `signature_parameter`, `decorated_parameter`, `callable_applicability`, `overload_selection`, `member_selection`, `dispatch_outcome`, `dispatch_target`, `member_family`, `member_family_edge`, `python_hierarchy_fact`, `occurrence`, `lexical_scope`, `binding`, `qualified_path`, `path_segment`, `state_event`, `branch_relation`, `cpp_expression`, `loop_relation`, `failure_handler_state`, `statement_reachability`, `pending_cleanup_flow` -> `file` | none | The workspace file that holds the row. Input list from the MCP help at `crates/bifrost-rql/src/query/schema.rs:1035–1043` and `crates/bifrost-rql/src/query/ir.rs:635–698`. |
| `imports_of` | `imports-of` | `file` -> `file` | none | Project files that the input file's resolved imports point to. For Rust, this includes the file that defines an item reached through `pub use` re-exports. |
| `importers_of` | `importers-of` | `file` -> `file` | none | Project files whose resolved imports point to the input file. For Rust, a file that reaches an item in the input file through `pub use` re-exports is an importer. |
| `python_import_site` | `python-import-site`, `python_import_site` | `structural_match` -> `structural_match` | none | Add parser-derived Python import leaves, local target resolution, scope, duplicate, exact `except ImportError` handler context, self-import, and `__all__` facts. |
| `python_import_cycles` | `python-import-cycles`, `python_import_cycles` | `structural_match` -> `structural_match` | none | Mark Python import sites with a confirmed cycle in the persisted workspace file graph; unresolved targets and absent paths remain incomplete. |
| `python_commented_out_code` | `python-commented-out-code`, `python_commented_out_code` | `structural_match` -> `python_rule_fact` | none | Return one source-spanned row for each Python commented-out-code block recognized from tree-sitter comment nodes. Rows retain exact comment lines and text; CodeQL-style example-heavy, documentation-like, filler, path, URL, doctest, and end-of-line exclusions apply. |

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
| `callees` | `callees` | `declaration` or `procedure` -> same row | `depth`, `proof`, `completeness` (only `exhaustive`) | Callees through resolved outgoing call edges, direct or up to `depth` hops. Procedure inputs retain the procedure domain (MCP help at `crates/bifrost-rql/src/query/schema.rs:892`). |
| `call_sites_to` | `call-sites-to`, `call_sites_to` | `declaration` -> `call_site` | `proof` | Call sites whose resolved callee is the declaration. |
| `call_sites_from` | `call-sites-from`, `call_sites_from` | `declaration` -> `call_site` | `proof` | Call sites written inside the declaration. |
| `call_input` | `call-input`, `call_input` | `call_site` -> `expression_site` | exactly one of `:receiver`, `:parameter-index`, `:parameter-name` | The receiver expression, or the argument bound to one formal parameter. |

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
| `occurrences_in` | `occurrences-in`, `occurrences_in` | `structural_match`, `call_shape`, `lexical_scope`, or `file` -> `occurrence` | `class`, `role`, `namespace` | Classified identifier occurrences inside the match or file, or whose exact innermost scope is the supplied lexical scope. Call-shape input is described in MCP help at `crates/bifrost-rql/src/query/schema.rs:969`. |
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
| `resolved_call` | `resolved-call`, `resolved_call` | `call_binding` -> `call_binding` | `:resolves-to` / `:resolves_to`, `:proof` (both required); `:receiver-type` | Keep binding rows whose call resolves to one callable identity with `exact` or `declared` proof. |
| `resolved_value` | `resolved-value`, `resolved_value` | `call_binding` or `structural_match` -> same row | `:resolves-to` / `:resolves_to`, exact `:proof` (both required) | Keep a bound call actual, Python decorator expression, Python structured value node, or C# structured value node only when the bounded resolver proves the requested exact declaration identity. Unresolved, ambiguous, shadowed, unsupported, or budget-limited values make the query incomplete. |
| `call_argument` | `call-argument`, `call_argument` | `call_binding` -> `call_binding` | exactly one of `:formal-name` / `:formal_name`, `:formal-index` / `:formal_index`, `:formal-annotation-type` / `:formal_annotation_type` | Keep the source argument bound to one formal parameter, with exact mapping and exhaustive coverage. |
| `cpp_bad_strncpy_size` | `cpp-bad-strncpy-size`, `cpp_bad_strncpy_size` | `call_binding` -> `structural_match` | none | Return the source call match when structured fixed-array extents prove the `strncpy` source is larger than its destination and the policy selected the exact declared library identity. See MCP help at `crates/bifrost-rql/src/query/schema.rs:948`. |
| `cpp_network_to_host_function_as_array_bound` | `cpp-network-to-host-function-as-array-bound`, `cpp_network_to_host_function_as_array_bound` | `call_binding` -> `structural_match` | none | Return the source call match when structured syntax proves a direct `ntohl` result is used as the `memcpy` bound after exact declared call selection. See MCP help at `crates/bifrost-rql/src/query/schema.rs:949`. |
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
| `procedure_effects` | `procedure-effects`, `procedure_effects` | `declaration` or `procedure` -> `procedure_effect` | none | One row per procedure and effect over its reachable call graph, direct or transitive, with coverage. Procedure input is described in MCP help at `crates/bifrost-rql/src/query/schema.rs:943`. |
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
| `program_point_by_id` | `program-point-by-id`, `program_point_by_id` | `procedure` -> `program_point` | `:id` (required) | Look up an observed content-scoped public point ID in the supplied current procedure. This selects a physical point and does not claim exhaustive AST/read discovery. Missing, stale, wrong-procedure, ambiguous or incomplete selection reports incomplete. IDs do not certify semantic-overlay freshness; downstream proofs acquire current evidence. |
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
| `cpp_inconsistent_loop_direction` | `cpp-inconsistent-loop-direction`, `cpp_inconsistent_loop_direction` | `structural_match` -> `structural_match` | none | Keep a C/C++ `for` match when its structured condition direction and update direction disagree for the same loop variable. |
| `cpp_unsafe_array_for_days_of_the_year` | `cpp-leap-year-unsafe-array-for-days-of-the-year`, `cpp_unsafe_array_for_days_of_the_year` | `structural_match` -> `structural_match` | none | Keep a C/C++ numeric-literal match when the structured literal `365` is a fixed array or vector-constructor extent. |
| `cpp_provenance` | `cpp-provenance`, `cpp_provenance` | `structural_match` or `call_binding` -> `structural_match` | `cpp_provenance_rule` | Filter a C/C++ return, assignment or call anchor through a direct procedure-local provenance rule. See MCP help at `crates/bifrost-rql/src/query/schema.rs:998`. |
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
| `value_flow` | `value-flow`, `value_flow` | `procedure` -> `flow_endpoint` | `:plan-ref` (required) | Run one host-registered value-flow plan for the exact procedure. |
| `taint` | `taint` | `procedure` -> `taint_finding` | `:taint-ref` (required) | Project the findings of one host-registered production taint result for the exact procedure. |
| `typestate` | `typestate` | `procedure` -> `typestate_finding` | `:protocol-ref` (required) | Run one host-registered typestate protocol for the exact procedure. |
| `witness` | `witness` | `typestate_finding`, `flow_endpoint`, or `absent_member_finding` -> `typestate_witness`, `flow_witness`, or `absent_member_witness`, respectively | `max_steps`, `max_bytes` | Project the retained evidence of each finding without running the analysis again. |
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
| `keyed_read_value` | `keyed-read-value`, `keyed_read_value` | `structural_match` -> `keyed_read_value` | `:runtime`, `:global`, `:container` (all required); exactly one of `:property`, `:index`, `:key-kind` / `:key_kind`; `index_min`, `index_max`, `source_origin` | A keyed read of a runtime input container, such as `process.env.HOME` or `sys.argv[1]`, resolved to its value. |
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

## Documented forms beyond the reference step tables

Source paths below are relative to the pinned Bifrost checkout. The public reference,
guide and tutorials describe the source, control and pattern forms. The additional
step descriptions are also published in the MCP `query_code` help and
`bifrost --help query_code`. The help renders the registry's signatures and
descriptions via `crates/bifrost-mcp/src/mcp_extended.rs:1170–1181` and
`crates/bifrost-rql/src/query/schema.rs:1046–1059`. Spellings for help-described
steps are taken from their RQL entries in the same schema file.

### Additional steps

| Form | Use | Input -> output | Source | Meaning |
| --- | --- | --- | --- | --- |
| `configuration-absence`, `configuration_absence` | step | `configuration_fact` -> `configuration_fact` | `docs/src/content/docs/reference/code-query.md:1800`, `crates/bifrost-rql/src/query/schema.rs:847` | Retain any configuration fact when no authored fact in the selected document or subtree matches the filter. |
| `python-unsafe-cyclic-import`, `python_unsafe_cyclic_import` | step | `structural_match` -> `python_rule_fact` | `crates/bifrost-rql/src/query/schema.rs:871` | Report module-level Python member uses and from-imports reached through a persisted import cycle before the target module binds that name. |
| `python-documentation`, `python_documentation` | step | `declaration` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:102`, `crates/bifrost-rql/src/query/schema.rs:872` | Report Python module, class, function, and method documentation facts, including public scope, decoded docstring, CodeQL-style code-line count, override status, property-accessor status, tri-state test scope and script entry-point status, and typed incompleteness. |
| `python-useless-class`, `python_useless_class` | step | `declaration` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:244`, `crates/bifrost-rql/src/query/schema.rs:873` | Report Python class method counts and the inheritance, subclass, special-method, decorator, mixin, statefulness, and top-level exclusions used by the useless-class policy, with typed incompleteness. |
| `python-attribute-shadows-method`, `python_attribute_shadows_method` | step | `declaration` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:460`, `crates/bifrost-rql/src/query/schema.rs:874` | Report a Python descendant method hidden by an attribute written through an ancestor initializer's self value. |
| `python-modification-of-default-value`, `python_modification_of_default_value` | step | `declaration` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:618`, `crates/bifrost-rql/src/query/schema.rs:875` | Report local mutations that can reach a list, dictionary, or set parameter default through direct local aliases. |
| `python-overly-complex-delete`, `python_overly_complex_delete` | step | `declaration` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:749`, `crates/bifrost-rql/src/query/schema.rs:876` | Report the reachable-block cyclomatic complexity of Python __del__ methods, pruning only exceptional paths and call continuations that exact semantic and handler evidence proves unlikely; unknown raised types retain their paths. |
| `class-hierarchy`, `class_hierarchy` | step | `declaration` -> `python_hierarchy_fact` | `docs/src/content/docs/code-query-tutorials/python.md:5561`, `crates/bifrost-rql/src/query/schema.rs:879` | Report each selected Python class's typed C3 hierarchy outcome and definite per-base inference_failure facts for unresolved or non-class bases, retaining reason and coverage. |
| `class-bases`, `class_bases` | step | `declaration` -> `python_hierarchy_fact` | `docs/src/content/docs/code-query-tutorials/python.md:5659`, `crates/bifrost-rql/src/query/schema.rs:880` | Return ordered Python runtime direct bases, including the implicit object base, retaining canonical source/external identity, model premises, and unresolved or non-class bases as typed facts. |
| `class-mro`, `class_mro` | step | `declaration` -> `python_hierarchy_fact` | `docs/src/content/docs/code-query-tutorials/python.md:5774`, `crates/bifrost-rql/src/query/schema.rs:881` | Return source and modeled external classes in a complete Python C3 linearization in MRO order with retained ancestry premises; incomplete hierarchies emit no partial MRO rows. |
| `mro-members`, `mro_members` | step | `declaration` -> `python_hierarchy_fact` | `docs/src/content/docs/code-query-tutorials/python.md:6070`, `crates/bifrost-rql/src/query/schema.rs:882` | Return known effective Python source members selected by C3 order, with owner and rank; unknown external member surfaces keep inventory, branch counts, and dependent protocol negatives open. |
| `python-overrides`, `python_overrides` | step | `declaration` -> `python_hierarchy_fact` | `docs/src/content/docs/code-query-tutorials/python.md:6205`, `crates/bifrost-rql/src/query/schema.rs:883` | Return one typed outcome per Python method; an override row names the nearest inherited same-name method selected by C3. |
| `python-call-relations`, `python_call_relations` | step | `declaration` -> `python_hierarchy_fact` | `docs/src/content/docs/code-query-tutorials/python.md:981`, `crates/bifrost-rql/src/query/schema.rs:884` | Report exact Python superclass lifecycle calls and super/delete context checks using C3, call shapes, bindings, and bounded dispatch; open evidence stays incomplete. |
| `points-to-candidates`, `points_to_candidates` | step | `receiver_analysis` -> `points_to_candidate` | `crates/bifrost-rql/src/query/schema.rs:904` | Expand the exact semantic object candidates retained by points_to before receiver/type projection can merge them, preserving each candidate's stable object identity, cardinality, proof, completeness, and set coverage. |
| `exact-external-callee`, `exact_external_callee` | step | `call_shape` -> `call_shape` | `crates/bifrost-rql/src/query/schema.rs:906` | Attach the canonical Python external callee proved by the language resolver for a call shape, when available. |
| `procedure-return-profile`, `procedure_return_profile` | step | `procedure` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:4007`, `crates/bifrost-rql/src/query/schema.rs:914` | Project one typed callable return profile from reachable ProcedureReturn events. |
| `call-result-use`, `call_result_use` | step | `call_shape` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:4100`, `crates/bifrost-rql/src/query/schema.rs:924` | Classify an exact Python call result as consumed, discarded, or open and include its resolved local callable return contract when proven. |
| `tuple-return-groups`, `tuple_return_groups` | step | `procedure` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:4499`, `crates/bifrost-rql/src/query/schema.rs:928` | Group exact Python tuple-return arities by callable, follow only supported local tuple identities, and preserve annotation exclusion and incomplete flow evidence. |
| `lambda-forwarding`, `lambda_forwarding` | step | `structural_match` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:5100`, `crates/bifrost-rql/src/query/schema.rs:929` | Prove whether an exact Python lambda forwards its positional parameters unchanged to one exact local callable; changed arguments and unsupported or ambiguous forms remain explicit. |
| `python-raise-legality`, `python_raise_legality` | step | `structural_match` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:7841`, `crates/bifrost-rql/src/query/schema.rs:933` | Classify the exact operand of a Python raise statement as a proven exception class/instance, a proven non-exception value, or open when resolver or hierarchy evidence is incomplete. |
| `python-special-method-raise`, `python_special_method_raise` | step | `structural_match` -> `python_rule_fact` | `docs/src/content/docs/code-query-tutorials/python.md:7911`, `crates/bifrost-rql/src/query/schema.rs:934` | Compare an exact direct Python raise operand in a special method with CodeQL's documented exception families, retaining open results for incomplete CFG, hierarchy, handler, or inherited-method evidence. |
| `cpp-non-constant-format`, `cpp_non_constant_format` | step | `call_binding` -> `structural_match` | `crates/bifrost-rql/src/query/schema.rs:950` | Retain exact modeled C/C++ printf-family calls whose format argument is not a direct constant string literal, projected back to the source call seed. |
| `cpp-wrong-number-format-arguments`, `cpp_wrong_number_format_arguments` | step | `call_binding` -> `structural_match` | `crates/bifrost-rql/src/query/schema.rs:951` | Retain exact modeled C/C++ printf-family calls whose complete literal format requires more variadic arguments than the call supplies, projected back to the source call seed. |
| `cpp-too-many-format-arguments`, `cpp_too_many_format_arguments` | step | `call_binding` -> `structural_match` | `crates/bifrost-rql/src/query/schema.rs:952` | Retain exact modeled C/C++ printf-family calls that supply more variadic arguments than their complete literal format consumes, projected back to the source call seed. |
| `cpp-wrong-type-format-argument`, `cpp_wrong_type_format_argument` | step | `call_binding` -> `structural_match` | `crates/bifrost-rql/src/query/schema.rs:953` | Retain exact modeled C/C++ printf-family calls whose structured literal or parameter type is incompatible with a parsed conversion directive, projected back to the source call seed. |
| `cpp-non-portable-printf`, `cpp_non_portable_printf` | step | `call_binding` -> `structural_match` | `crates/bifrost-rql/src/query/schema.rs:954` | Retain exact modeled C/C++ printf calls whose structured argument type depends on a non-portable integer width or pointer format contract, projected back to the source call seed. |
| `cpp-overflowing-snprintf`, `cpp_overflowing_snprintf` | step | `call_binding` -> `structural_match` | `crates/bifrost-rql/src/query/schema.rs:955` | Retain modeled snprintf calls only when a future bounded destination-extent and return-flow producer proves overflow; missing extent or flow evidence is incomplete. |
| `literal-occurrences`, `literal_occurrences` | step | `structural_match` -> `literal_occurrence` | `crates/bifrost-rql/src/query/schema.rs:957` | Project one query-local scalar literal occurrence for each selected literal fact. |
| `callable-metrics`, `callable_metrics` | step | `declaration` -> `callable_metrics` | `crates/bifrost-rql/src/query/schema.rs:958` | Project query-local structured cognitive complexity, maximum executable nesting, and owned executable span metrics for each source-backed callable. |
| `python-comparison-constants`, `python_comparison_constants` | step | `structural_match` -> `structural_match` | `docs/src/content/docs/code-query-tutorials/python.md:8127`, `crates/bifrost-rql/src/query/schema.rs:977` | Retain Python comparisons whose two AST operands are supported constants: scalar and parenthesized literals, constant tuples, constant string concatenation, and unary not, numeric signs, or bitwise inversion over constants. |
| `python-comparison-using-is`, `python_comparison_using_is` | step | `structural_match` -> `structural_match` | `docs/src/content/docs/code-query-tutorials/python.md:8416`, `crates/bifrost-rql/src/query/schema.rs:978` | Retain Python is/is-not comparisons when a resolved operand has value equality through a builtin value type or an effective custom __eq__ in its complete MRO. |
| `python-comparison-using-is-non-portable`, `python_comparison_using_is_non_portable` | step | `structural_match` -> `structural_match` | `docs/src/content/docs/code-query-tutorials/python.md:8724`, `crates/bifrost-rql/src/query/schema.rs:979` | Retain Python is/is-not comparisons against integer or string literals that CPython interns but Python implementations do not universally intern. |
| `binding-initializer-calls-of`, `binding_initializer_calls_of` | step | `state_event` -> `binding_initializer_call` | `crates/bifrost-rql/src/query/schema.rs:989` | For one Rust binding establishment, project exact call sites on the initializer result and method-receiver chain. |
| `pending-cleanup-flows`, `pending_cleanup_flows` | step | `procedure` -> `pending_cleanup_flow` | `crates/bifrost-rql/src/query/schema.rs:1001` | Derive the effect of each lowered finally, ensure, defer, using, with, or resource cleanup on a pending return, throw, break, or continue. |
| `rust-marker-impls-of`, `rust_marker_impls_of` | step | `structural_match` or `declaration` -> `rust_marker_impl` | `docs/src/content/docs/code-query-tutorials/rust.md:722–737`, `crates/bifrost-rql/src/query/schema.rs:1002` | Enumerate same-file Rust Send/Sync impl parameters, marker bounds, field wrapper uses, shared-reference access, and completeness proofs using bounded syntax and definition lookup. |
| `rust-pointer-findings`, `rust_pointer_findings` | step | `procedure` -> `rust_pointer_finding` | `crates/bifrost-rql/src/query/schema.rs:1003` | Run query-local Rust pointer provenance and project accesses whose same-origin scope-end, invalidation, or checked byte extent precedes the access. |

### Sources, controls and composition

`same row` preserves the input domain. Typed set operators preserve the branch
domains. `call-reachability` uses the operand kinds shown per option.
For `correlate`, the relation determines the code-side domain. The
configuration operand is always a `configuration_fact` query.
`require-both` accepts a programming-code query on the code side.
`explain` describes a plan and has no result-row domain.

| Form | Use | Input -> output | Source | Meaning |
| --- | --- | --- | --- | --- |
| `occurrences`, `occurrence` | source | none -> `occurrence` | `docs/src/content/docs/reference/code-query.md:55` | Seed occurrence rows. |
| `scopes`, `scope` | source | none -> `lexical_scope` | `docs/src/content/docs/reference/code-query.md:56` | Seed lexical scope rows. |
| `bindings`, `binding` | source | none -> `binding` | `docs/src/content/docs/reference/code-query.md:57` | Seed binding rows. |
| `paths`, `path` | source | none -> `qualified_path` | `docs/src/content/docs/reference/code-query.md:58` | Seed qualified path rows. |
| `generation-sites`, `generation_sites` | source | none -> `generation_site` | `docs/src/content/docs/reference/code-query.md:59` | Seed generation site rows. |
| `exports`, `export` | source | none -> `export` | `docs/src/content/docs/reference/code-query.md:60` | Seed export rows. |
| `dependency-evidence`, `dependency_evidence` | source | none -> `dependency_evidence` | `docs/src/content/docs/reference/code-query.md:61` | Seed dependency evidence rows. |
| `configuration-facts`, `configuration_facts` | source | none -> `configuration_fact` | `docs/src/content/docs/reference/code-query.md:62` | Seed configuration fact rows. |
| `where` | step | any row -> same row | `docs/src/content/docs/reference/code-query.md:63` | Restrict the workspace paths. |
| `language`, `languages` | step | any row -> same row | `docs/src/content/docs/reference/code-query.md:64` | Restrict the languages. |
| `limit` | step | any row -> same row | `docs/src/content/docs/reference/code-query.md:69` | Limit terminal results. |
| `result-detail`, `result_detail` | step | any row -> same row | `docs/src/content/docs/reference/code-query.md:70` | Select row detail. |
| `profile` | step | any row -> same row | `docs/src/content/docs/reference/code-query.md:71` | Attach execution measurements to ordinary results. |
| `explain` | step | any row -> unknown | `docs/src/content/docs/reference/code-query.md:71` | Describe the plan without executing it. |
| `inside` | ancestor | `structural_match` -> same row | `docs/src/content/docs/reference/code-query.md:65` | Require a matching ancestor. |
| `inside-decl`, `inside_decl` | ancestor | `structural_match` -> same row | `docs/src/content/docs/reference/code-query.md:66` | Require a matching ancestor within the nearest callable boundary. |
| `not-inside` | ancestor | `structural_match` -> same row | `docs/src/content/docs/reference/code-query.md:67` | Exclude a matching ancestor. |
| `union` | set | branch rows -> same row | `docs/src/content/docs/reference/code-query.md:52`, `docs/src/content/docs/rune-query-language.md:278` | Keep the endpoints from any branch. Mixed row kinds are allowed at the root and through filters or projection. |
| `intersect` | set | branch rows -> same row | `docs/src/content/docs/reference/code-query.md:53`, `docs/src/content/docs/rune-query-language.md:278` | Keep endpoints common to every branch of the same kind. |
| `except` | set | branch rows -> same row | `docs/src/content/docs/reference/code-query.md:54`, `docs/src/content/docs/rune-query-language.md:278` | Subtract later branch endpoints of the same kind from the first. |
| `call-reachability` | compound | :sources = `declaration` or `structural_match`; :sinks = `structural_match`, `call_site`, or `dispatch_target` -> `declaration` | `docs/src/content/docs/rune-query-language.md:309–325` | Keep source callables with certified paths to selected sinks. |
| `correlate` | compound | :configuration = `configuration_fact` -> `cross_domain` | `docs/src/content/docs/code-query-json.md:207–239`, `docs/src/content/docs/reference/code-query.md:1746–1787` | Pair rows via a reviewed relation. |
| `require-both`, `require_both` | compound | :code = any code row; :configuration = `configuration_fact` -> `cross_domain` | `docs/src/content/docs/code-query-json.md:250–260` | Emit contributing rows if both sides return rows. |
| `configuration-key`, `configuration_key` | relation | :code = `expression_site` -> n/a | `docs/src/content/docs/code-query-json.md:209–239` | Match an exact call argument to a properties key. |
| `configuration-code`, `configuration_code` | relation | :code = any code row -> n/a | `crates/bifrost-mcp/src/mcp_extended.rs:1036–1055`, `crates/bifrost-rql/src/query/schema.rs:2724–2729,3951` | Join authored configuration to a code declaration or an exact same-project code row. Scalar matches require `declaration` rows, while `:match same-project` accepts programming-code rows. |
| `dependency-configuration`, `dependency_configuration` | relation | :code = `dependency_evidence` -> n/a | `docs/src/content/docs/reference/code-query.md:1746–1787` | Match a dependency to an explicitly reviewed configuration mapping. |

### Structural pattern heads and predicates

Normalized kinds and standalone pattern predicates produce `structural_match`
rows when they anchor a query. Nested patterns constrain matching and have no
separate row domain. The normalized kind list is published in the MCP pattern
description at `crates/bifrost-mcp/src/mcp_extended.rs:1154–1165`.
The reference describes common pattern roles at
`docs/src/content/docs/reference/code-query.md:236–257`, with named argument
pairs and list-valued roles illustrated at
`docs/src/content/docs/rune-query-language.md:115–119`.

| Form | Use | Input -> output | Source | Meaning |
| --- | --- | --- | --- | --- |
| `declaration` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:74` | Match any declaration, including callable, class, and import declarations. |
| `callable` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:75` | Match any callable declaration, including functions, methods, constructors, and lambdas. |
| `function` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:76` | Match a named free-standing function declaration. |
| `method` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:77` | Match a method declared on a class or similar container. |
| `constructor` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:78` | Match a constructor declaration. |
| `lambda` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:79` | Match an anonymous function or lambda expression. |
| `class` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:80` | Match a class-like declaration. |
| `import` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:81` | Match an import declaration. |
| `parameter` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:82` | Match a formal parameter declaration. |
| `module` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:83` | Match a module or namespace declaration. |
| `call` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:85` | Match call expressions for functions, methods, or constructors. |
| `concurrent_spawn` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:86` | Match a structured operation that starts a callable in a distinct concurrent task. |
| `resource_release` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:87` | Match a language construct that releases a resource at its own exit, such as a Java try-with-resources statement or a Python with statement. |
| `assignment` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:88` | Match an assignment expression or statement. |
| `field_access` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:89` | Match member or field access. |
| `identifier` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:90` | Match an identifier reference. |
| `literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:91` | Match any literal value. |
| `string_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:92` | Match a string literal. |
| `numeric_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:93` | Match a numeric literal. |
| `boolean_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:94` | Match a boolean literal. |
| `null_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:95` | Match a null-like literal. |
| `collection_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:96` | Match a collection display literal such as an array, list, set, dictionary, or tuple literal. |
| `jsx_element` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:100` | Match a JSX element, including a self-closing element. |
| `jsx_attribute` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:101` | Match a named JSX attribute. |
| `jsx_spread_attribute` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:102` | Match a JSX spread attribute expression. |
| `object_property` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:103` | Match a named object-literal property pair. |
| `computed_property` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:104` | Match a computed object-property key. |
| `spread_element` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:105` | Match a structured spread expression in an object, array, or argument list. |
| `return` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:107` | Match a return statement. |
| `throw` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:108` | Match a throw or raise statement. |
| `catch` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:109` | Match a catch or exception-handler clause. |
| `if` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:110` | Match a conditional statement or expression. |
| `loop` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:111` | Match a loop construct. |
| `for_loop` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:112` | Match a loop that iterates a collection, iterator, or range (the for-each family). |
| `while_loop` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:113` | Match a condition-controlled loop (while, do-while, or until). |
| `decorator` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:114` | Match a decorator or annotation. |
| `block` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:115` | Match a statement list that forms a lexical scope of its own, such as a method body, a bare block, or a switch body. |
| `await` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:117` | Match an await expression that suspends asynchronous execution. |
| `comment` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:119` | Match a source comment token. |
| `syntax_error` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:120` | Match a source span reported by parser error recovery. |
| `binary_expression` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:123` | Match a binary expression with grammar-backed left, right, and operator roles. |
| `comparison` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:124` | Match a comparison expression, including a pairwise comparison in a comparison chain. |
| `comparison_chain` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:125` | Match a chained comparison whose pairwise operands and operators are not exposed as singular roles. |
| `assertion` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:126` | Match an assertion statement. |
| `regex_pattern` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:129` | Match a decoded Python regular-expression pattern value. |
| `regex_sequence` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:130` | Match a sequence of Python regular-expression terms. |
| `regex_group` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:131` | Match a group in a Python regular-expression pattern. |
| `regex_alternation` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:132` | Match an alternation in a Python regular-expression pattern. |
| `regex_character_class` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:133` | Match a character class in a Python regular-expression pattern. |
| `regex_character_range` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:134` | Match a character range in a Python regular-expression class. |
| `regex_quantifier` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:135` | Match a quantifier in a Python regular-expression pattern. |
| `regex_anchor` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:136` | Match an anchor or boundary escape in a Python regular-expression pattern. |
| `regex_escape` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:137` | Match an escape in a Python regular-expression pattern. |
| `regex_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:138` | Match a literal character in a Python regular-expression pattern. |
| `regex_wildcard` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:139` | Match a wildcard atom in a Python regular-expression pattern. |
| `regex_risk_hostname` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:140` | Match an unescaped-dot fact in a hostname-like regex pattern. |
| `regex_risk_range` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:141` | Match an overly broad ASCII character-range fact. |
| `regex_risk_bad_tag` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:142` | Match a bounded HTML tag-filter language-coverage risk. |
| `regex_risk_exponential` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:143` | Match a statically identified exponential backtracking risk. |
| `regex_incomplete` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:144` | Match a regex analysis that could not establish a complete result. |
| `regex_risk_backspace_escape` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:145` | Match a backspace escape inside a Python regex character class. |
| `regex_risk_duplicate_class_character` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:146` | Match a duplicate literal member in a Python regex character class. |
| `regex_risk_unmatchable_caret` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:147` | Match a caret anchor that follows a consuming term in one regex branch. |
| `regex_risk_unmatchable_dollar` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:148` | Match a dollar anchor that precedes a consuming term in one regex branch. |
| `regex_risk_incomplete_special_group` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:149` | Match an unsupported or incomplete Python regex special group. |
| `tuple_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:151` | Match a tuple display literal, preserving tuple identity and ordered elements. |
| `list_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:152` | Match a list display literal, preserving its ordered direct elements. |
| `regex_risk_polynomial` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:153` | Match a statically identified polynomial-time Python regex backtracking risk. |
| `switch` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:156` | Match a switch statement. |
| `case` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:157` | Match a switch case or default label. |
| `label` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:158` | Match a labeled statement target. |
| `goto` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:159` | Match a goto statement. |
| `break` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:160` | Match a break statement. |
| `continue` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:161` | Match a continue statement. |
| `character_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:162` | Match a character literal. |
| `yield` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:163` | Match a yield expression or statement that controls generator execution. |
| `cast` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:164` | Match a cast expression. |
| `instance_of` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:165` | Match a type-test expression. |
| `subscript` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:166` | Match a subscript expression. |
| `array_creation` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:167` | Match an array creation expression. |
| `unary_expression` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:168` | Match a unary expression. |
| `lock` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:169` | Match a synchronized or lock statement. |
| `type_literal` | pattern | none -> `structural_match` | `crates/bifrost-core/src/analyzer/structural/kinds.rs:170` | Match an expression whose value is a written type, such as C# `typeof(T)`; its `type` role is the type operand. |
| `not-kind`, `not_kind` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:132` | Exclude listed kinds and their subtypes. |
| `modifier` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:133` | Match a recorded declaration modifier. |
| `async` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:134` | Match recorded callable asyncness. |
| `name` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:135` | Match the normalized name. |
| `name/regex` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:136` | Match the normalized name with a regular expression. |
| `text/regex` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:137` | Match parser-backed text with a regular expression. |
| `boolean-value`, `boolean_value` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:138` | Match the decoded Boolean value. |
| `value`, `string-value`, `string_value` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:139` | Match the decoded string value. |
| `value/regex`, `string-value/regex`, `string_value/regex` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:140` | Match the decoded string value with a regular expression. |
| `not`, `and`, `or` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:141` | Compose constraints on the same node. |
| `capture` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:142` | Bind this matched node to a capture label. |
| `arity` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:143` | Match a count of arguments or elements. |
| `count` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:144` | Count direct children matching a pattern. |
| `visibility` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:147` | Match recorded callable visibility. |
| `parameter-type`, `parameter_type` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:148` | Match a parameter type spelling. |
| `parameter-type/regex`, `parameter_type/regex` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:149` | Match a parameter type spelling with a regular expression. |
| `has` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:150` | Require a matching descendant. |
| `each` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:151` | Match this node or descendants and collect captures. |
| `argument-mode`, `argument_mode` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:152` | Match the passing mode on an argument edge. |
| `not-has`, `not_has` | pattern | none -> `structural_match` | `docs/src/content/docs/reference/code-query.md:153` | Exclude nodes with a matching descendant. |
| `unsafe-context`, `unsafe_context` | pattern | none -> `structural_match` | `docs/src/content/docs/code-query-tutorials/rust.md:172–182` | Match calls inside or outside a Rust unsafe context. |
| `parenthesized` | pattern | none -> `structural_match` | `docs/src/content/docs/code-query-tutorials/python.md:2341–2352` | Match a parenthesized Python string concatenation. |
| `language-rule`, `language_rule` | pattern | none -> `structural_match` | `docs/src/content/docs/code-query-tutorials/csharp.md:395–406` | Apply a registered language-specific AST predicate. |
| `body-state`, `body_state` | pattern | none -> `structural_match` | `docs/src/content/docs/code-query-tutorials/csharp.md:477–489` | Match the structured state of a branch or loop block. |
| `body-owner`, `body_owner` | pattern | none -> `structural_match` | `docs/src/content/docs/code-query-tutorials/csharp.md:477–489` | Match the owner role of a branch or loop block. |
| `instantiated-type`, `instantiated_type`, `qualified` | operand | n/a | `docs/src/content/docs/rune-query-language.md:347–363` | Constrain a Rust call qualifier by canonical type and selected type arguments. |
| `field` | operand | n/a | `docs/src/content/docs/rune-query-language.md:423–429` | Name the second field in a row comparison. The documented comparison operators are `eq`, `ne`, `lt`, `le`, `gt`, `ge`, `is-null`, `is-not-null` and `in`. |
| `key`, `index` | operand | n/a | `docs/src/content/docs/reference/code-query.md:977–985` | Select a key or array index in a configuration route. |

## Composition

A query starts from a structural pattern (normalized kind or name/text predicate anchor), or from a dedicated source form such as `occurrences`, `scopes`, `bindings`, `paths`, `generation-sites`, `exports`, and configuration/dependency evidence. Sources emit their own row domain; they are not transformations over an implicit whole-workspace row set. A step wraps the query producing its input, so inner forms run first; the resulting ordered pipeline must have compatible adjacent input/output row domains. Set branches combine query results; typed continuations require compatible terminal domains, and a common suffix can follow the set. `filter` retains rows meeting every typed predicate; `project` publishes selected/renamed public fields and thereafter only filter/project can follow. Limits include query/pattern/source budgets and step-specific bounds (depth, witness caps, etc.); they bound evidence and can make results incomplete, not prove absence. Root `explain` lowers/selects without scanning; `profile` executes and reports measurements.

## Candidate missing-step facts (pin-specific)

- **Value flow:** documented, not grammar-only or absent. `value-flow` takes a `procedure` plus required `:plan-ref "namespace:name"` and emits endpoints. It runs one immutable plan registered by the host; the RQL author selects a registered plan, not an arbitrary procedure-level flow algorithm. The registry implementation and plan catalog are outside the RQL parser/reference; the reference example names `embedding:request-to-sink`, and other names depend on host registrations. Thus it cannot be assumed that every workspace/procedure has endpoints: only procedures analyzed under the selected registered plan can return them. The public syntax supports Rust only to the extent the host plan/provider supports Rust; the reference does not promise Rust support for arbitrary plans. The registry contract is explicit in `crates/bifrost-rql/src/query/schema.rs` near lines 860, 3810 and 590-640. The reference's value-flow section is near line 535.
- **Field writes and their values:** both documented. `field-writes-of` enumerates writes to an indexed field declaration; it has Rust field-store support subject to semantic IR lowering and explicit completeness conditions. `field-write-value` projects the RHS for a proven receiver/member target; it is documented, but its public reference does not state Rust language support, so support is not established there. Reference lines near 472 and 559 (verify these line numbers against the relevant table in this pinned file when using the claim).
- **Argument values at call sites:** documented as `call-input`, from a `call_site` to the receiver expression or argument bound to a formal parameter. Rust argument extraction follows whatever call/binding facts the Rust adapter emits; the RQL reference does not claim general Rust coverage, so this is not evidence of universal Rust support. See `docs/src/content/docs/reference/code-query.md` around the call step table and `docs/src/content/docs/rune-query-language.md` around line 373.
- **Callers/callees:** documented, neither grammar-only nor absent. `callers` and `callees` traverse resolved incoming/outgoing call edges, optionally by depth, with proof/completeness controls. Rust works where indexed resolved call edges exist; the reference does not certify exhaustive Rust call-graph coverage, so callers/callees results retain their proof/completeness qualifications. Reference lines 382-383; Rust-specific coverage caveats are in the RQL guide near lines 283-287.

### Documentation boundary

The MCP tool's top-level discovery description is deliberately generic; its generated `steps` parameter carries every step's typed signature and prose description. The generated help and the public docs establish documentation. The parser confirms spellings. The ten-minute evaluation fixture demonstrates a small structural query and policy selector; it does not define additional step semantics.
