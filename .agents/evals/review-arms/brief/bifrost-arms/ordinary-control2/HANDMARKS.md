# Ordinary Control2 semantic reliance marks

All eight broad-match rejected BT findings were read in full alongside their six frozen briefs. Seven build on or restate a stated brief relation; one shares generated-file anchors without the claimed property-gap uniqueness relation. The decision is not proof of what a reviewer read or that a brief relation is false. Repeated findings in the same verifier group count separately. Raw verifier labels remain unchanged.

## f-8e5f7492 — true

- [P2] Revalidate the artifact when reusing its dispatch session — crates/bifrost-analysis/src/analyzer/semantic/icfg.rs:1377-1384. If a file or overlay changes after the first dispatch while the provider is still in use, a later dispatch for another call in the same artifact reuses the old source without calling `exact_source_for_procedure` again. Previously each direct dispatch checked the current artifact key and rejected a stale handle; the retained session instead combines old call ranges and syntax with current analyzer lookups, which can produce incorrect dispatch results. The same issue affects uncached call transfers through this session.

Builds on the brief's explicit artifact-keyed retained session shared across distinct calls, plus retained exact-source lifetime, then alleges missing per-call revalidation. Those are the substantive reuse/source relations in its failure claim; this mark does not accept the rejected stale-handle claim or prove actual reviewer dependence.

Brief line 6:
> - Typestate dependency discovery iterates each procedure's call sites using one supplied request and invokes `call_transfers` for each (`crates/bifrost-flow/src/typestate/production.rs:1147`); distinct calls in the same artifact reach the same session lookup (`crates/bifrost-analysis/src/analyzer/semantic/icfg.rs:1477`).

Brief line 19:
> - Each session-map insertion clones the provider's oracle and retains the supplied artifact (`crates/bifrost-analysis/src/analyzer/semantic/icfg.rs:623`, `crates/bifrost-analysis/src/analyzer/semantic/workspace_oracle/dispatch.rs:439`). Exact source later comes from `current_artifact_source`, with workspace-mount and artifact-key comparisons before returning the source (`crates/bifrost-analysis/src/analyzer/semantic/workspace_oracle/dispatch.rs:2607`).

Brief line 22:
> - `retains_exact_source` reads `low_level_source_paid`, and `discard_exact_source` clears both that flag and the low-level session (`crates/bifrost-analysis/src/analyzer/semantic/workspace_oracle/dispatch.rs:172`). Ordinary dispatch sets the flag from committed source-byte work (`crates/bifrost-analysis/src/analyzer/semantic/workspace_oracle/dispatch.rs:262`); the declared-local-target route sets it after charging source and result work (`crates/bifrost-analysis/src/analyzer/semantic/workspace_oracle/dispatch.rs:386`).

## f-5c1a5886 — true

- [P2] Preserve current line endings for structural facts — crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:3383-3385. When indexed source uses LF and the working file uses CRLF, this returns the indexed text where the previous code returned the compatible working text. Structural fact extraction uses this source, so its byte ranges after the first line no longer point to the corresponding positions in the working file.

Builds on the brief's explicit flow from changed file_source into structural_facts_with_outcome/snapshot extraction and its indexed/current CRLF selection relation, then alleges incompatible working-file offsets. This is the stated producer-consumer/source-selection relation, rather than line overlap alone. The verification rejects the consequence, not the relevance of that relation.

Brief line 5:
> - `structural_facts_with_outcome` uses the changed `file_source` result for its snapshot key, cache lookup, and fact extraction (`crates/bifrost-analysis/src/analyzer/structural/provider.rs:411`, `crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:3382`).

Brief line 18:
> - `structural_file_state` compares indexed and current text after CRLF normalization. It retains indexed state for other text differences; otherwise, it requests current-source state (`crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:3747`, `crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:3773`).

## f-9d9bd027 — true

- [P2] Keep structural facts aligned with CRLF source — crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:3383-3385. When Git stores LF text but the working file uses CRLF, this returns the indexed LF text. Previously, `structural_file_state` selected the current CRLF text. Structural fact extraction therefore produces byte offsets that do not match the working file, so consumers can identify the wrong source span.

Same semantic relation as f-5c1a5886: changed file_source feeds structural fact extraction, combined with indexed/current CRLF selection. Its false offset consequence builds on those stated data-flow relations. Counted separately because the registered unit is a finding ID, not a duplicate group.

Brief line 5:
> - `structural_facts_with_outcome` uses the changed `file_source` result for its snapshot key, cache lookup, and fact extraction (`crates/bifrost-analysis/src/analyzer/structural/provider.rs:411`, `crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:3382`).

Brief line 18:
> - `structural_file_state` compares indexed and current text after CRLF normalization. It retains indexed state for other text differences; otherwise, it requests current-source state (`crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:3747`, `crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:3773`).

## f-2ba7ce5c — false

- [P2] Preserve distinct property gaps with the same reason and frontier — crates/bifrost-analysis/src/analyzer/store/resolution_stage/rows.rs:142-142. When a definition has property gaps that share a reason and frontier but differ in kind or source site, lowering treats them as distinct rows, while this unique index collapses those distinctions and rejects the second row. Include the remaining gap identity fields so all valid lowered gaps can be projected.

The claim concerns the uniqueness key of definition_property_gaps omitting kind/source-site identity. The nearby brief item describes call_obligations JSONB columns, and its general LoweredCoverageGap item concerns a different gaps table/index family. It does not state this property-gap uniqueness/collision relation; shared generated-file anchors and general schema/project callbacks alone are insufficient.

Brief line 15:
> - `LoweredCallApplicabilityObligation` carries argument slots, eligible rules and an applicability reason (crates/bifrost-analysis/src/analyzer/resolution/typed_fact_lowering.rs:570); the new `call_obligations` family gives the reason semantic columns and stores the two collections in JSONB-checked columns (crates/bifrost-analysis/src/analyzer/store/resolution_stage/rows.rs:145).

Brief line 20:
> - `LoweredCoverageGap` carries gap identity, reason semantic, site, origin and frontier (crates/bifrost-analysis/src/analyzer/resolution/fact_lowering.rs:270); the new gaps table exposes these dimensions and indexes subject, endpoint and reason independently (crates/bifrost-analysis/src/analyzer/store/resolution_stage/rows.rs:283).

## f-a84760e9 — true

- [P2] Follow alias chains before extracting wrapper arguments — src/analyzer/usages/get_definition.rs:7346-7347 — For `using Ptr = Wrapper<Node>; using Alias = Ptr;`, the receiver's alias is `Alias`, so this reads `Ptr` as the wrapper head and collects no arguments. Resolving the head follows the alias to `Wrapper`, but its `T` cannot be substituted and `Alias::operator->` member resolution fails; the template arguments need to be retained while following the alias chain.

Builds on the brief's explicit alias-RHS parsing into wrapper head/template arguments, recursive alias-target type resolution, and parameter-order substitution. The finding joins those stated relations to argue that an outer alias loses inner template arguments; its rejection as preexisting does not remove semantic reliance under the original rubric.

Brief line 15:
> - `cpp_alias_target_texts` reads the unit signature, analyzer signatures, and source; its parser extracts text after `=` or the type portion of a `typedef` (`src/analyzer/usages/get_definition.rs:7456`, `src/analyzer/usages/get_definition.rs:7468`). The arrow resolver splits this text into wrapper head and template arguments (`src/analyzer/usages/get_definition.rs:7345`).

Brief line 16:
> - Wrapper and substituted pointee names from the arrow resolver (`src/analyzer/usages/get_definition.rs:7349`, `src/analyzer/usages/get_definition.rs:7374`) enter `cpp_resolve_type_unit_inner`, which searches visible classes and aliases, follows alias targets, and chooses a canonical type (`src/analyzer/usages/get_definition.rs:7248`, `src/analyzer/usages/get_definition.rs:7269`).

Brief line 19:
> - `cpp_template_parameter_names` reads the wrapper signature before analyzer signatures (`src/analyzer/usages/get_definition.rs:7398`). Class collection assigns `scope.template_signature` to the class unit and preserves it in the nested member scope (`src/analyzer/cpp/declarations.rs:361`, `src/analyzer/cpp/declarations.rs:393`); parameter order supplies the argument index used by substitution (`src/analyzer/usages/get_definition.rs:7386`).

## f-7cc662ec — true

- [P1] Avoid resolving every Go import with a full file scan — src/analyzer/go_analyzer.rs:230-232. On a large Go repository, the first reverse lookup now calls `imported_code_units_of` for every file, and that resolver scans all analyzed files for each import. This makes even a single lookup quadratic in repository size, whereas the previous lookup first filtered candidates by the requested target and only performed that expensive resolution for matching importers. The new index can therefore make the cold lookup substantially slower or cause timeouts on the large repositories this change targets.

Builds on the brief's explicit shared builder invoking forward resolution for every analyzed file and its Go callback/forward-cache/provider relation, then alleges quadratic cold lookup relative to the old route. The new-versus-old performance claim is rejected, but it substantively uses the stated eager-builder-to-forward-resolver relation rather than merely sharing a source line.

Brief line 15:
> - The shared builder receives each analyzer's `get_analyzed_files()` set (`src/analyzer/go_analyzer.rs:231`); the underlying implementation collects keys from `state.files` (`src/analyzer/tree_sitter_analyzer.rs:639`). The builder invokes forward resolution for every member using parallel iteration (`src/analyzer/capabilities.rs:71`).

Brief line 18:
> - Go's builder callback (`src/analyzer/go_analyzer.rs:232`) reads the forward cache or resolves imports against analyzed file parents (`src/analyzer/go_analyzer.rs:185`). Resolution skips `_` aliases and includes non-module top-level declarations from matching files (`src/analyzer/go_analyzer.rs:193`, `src/analyzer/go_analyzer.rs:210`).

## f-240385f1 — true

- [P2] Include pointer depth in field type agreement — crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:8049-8053. When candidate fields have the same underlying class but different pointer depths, such as `T value` and `T* value` inherited from different bases, this comparison treats their types as equal and returns whichever declaration appears first. The resulting `CppType` retains that arbitrary indirection, so it can select an incorrect overload when the field expression is used as a call argument, and it also defeats the intended fail-closed handling of conflicting declarations.

Builds on the brief's relation from unanimous field typing to call-argument typing and its distinction between declared indirection and logical-symbol agreement. It alleges that this combination ignores pointer depth and chooses an arbitrary overload. Rejection because inherited-field behavior predates the change is separate from reliance on these stated relations.

Brief line 2:
> - `cpp_expression_type` routes field expressions to `cpp_field_expression_type` (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:7791), whose field candidates now pass through `cpp_unanimous_field_type` (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:7873); these expression types also become call argument types through `cpp_call_argument_types` (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:7749).

Brief line 16:
> - `cpp_unanimous_field_type` obtains types through `cpp_field_declared_type` (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:8040); that constructor reads normalized type text, a resolved unit and indirection from the shared declared-field fact resolver (crates/bifrost-cpp/src/graph/resolver.rs:10833), adds an alias unit and sets `pointee_const` to false (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:7891).

Brief line 17:
> - Both the type agreement check (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:8051) and qualified-variable identity check (crates/bifrost-cpp/src/graph/extractor.rs:7764) use `same_visible_symbol` (crates/bifrost-cpp/src/graph/resolver.rs:15056); its logical-symbol branch compares kind, fully qualified name and signature without comparing source files (crates/bifrost-cpp/src/graph/resolver.rs:15121).

## f-c2c1b518 — true

- [P2] Include indirection in field type agreement crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:8047-8049
  When two candidate fields have types such as `Helper` and `Helper*`, both resolve to the same `CodeUnit`, so this comparison treats them as agreeing and returns whichever `CppType` was encountered first. Consequently, an ambiguous inherited field or conflicting qualified declarations can still select an arbitrary receiver or call overload, even though pointer indirection is part of the type used by overload matching.

Same semantic relation as f-240385f1: field declarations carry indirection, agreement compares logical symbols, and field expression types feed call arguments. The pointer-agreement/overload allegation builds on those stated relations. This is a separate finding ID even though the verifier grouped the failure together.

Brief line 2:
> - `cpp_expression_type` routes field expressions to `cpp_field_expression_type` (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:7791), whose field candidates now pass through `cpp_unanimous_field_type` (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:7873); these expression types also become call argument types through `cpp_call_argument_types` (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:7749).

Brief line 16:
> - `cpp_unanimous_field_type` obtains types through `cpp_field_declared_type` (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:8040); that constructor reads normalized type text, a resolved unit and indirection from the shared declared-field fact resolver (crates/bifrost-cpp/src/graph/resolver.rs:10833), adds an alias unit and sets `pointee_const` to false (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:7891).

Brief line 17:
> - Both the type agreement check (crates/bifrost-analysis/src/analyzer/usages/get_definition/cpp.rs:8051) and qualified-variable identity check (crates/bifrost-cpp/src/graph/extractor.rs:7764) use `same_visible_symbol` (crates/bifrost-cpp/src/graph/resolver.rs:15056); its logical-symbol branch compares kind, fully qualified name and signature without comparing source files (crates/bifrost-cpp/src/graph/resolver.rs:15121).
