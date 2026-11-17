# Artifact-only regression diagnostic

Everydiff with lower caught share:4diffs,5netlostcatches. Override-familyisnotnegative(3vs3)andnotincludedasloss.

No new model calls. The primary reading remains fixed. Handbacks show reported topics; reviewer tool transcripts are unavailable, so attention/deference/stopping cannot be established.

## python-filescope-enclosing-tie

Caught A1/3 to BT0/3.

Brief lines18/20 direct attention to Java import-hit limits and file-wide Rust direct_names. Lines4–5 mention min-span lookup but not the real Python module/equal-span collision.

All3BT handbacks report Java import-limit and/or Rust import-binding problems, not the FileScope tie. Each adds a concrete failure claim; they are not just neutral relations restated as findings. Those other claims are not verified by this diagnostic.

Not observable: no handback states deference/stopping and reviewer tool transcripts are unavailable.

Site overlap is consistent with competing attention to brief-listed topics, but cannot establish where the reviewers actually looked or why they missed.

`briefs/blind/python-filescope-enclosing-tie/brief.md:5`:
> - Line-based enclosing lookup calls `self.declarations(file)` (`src/analyzer/tree_sitter_analyzer.rs:1430`), so it receives the file-scope-filtered iterator (`src/analyzer/tree_sitter_analyzer.rs:1329`), while byte-based lookup selects the smallest containing range from the underlying set (`src/analyzer/tree_sitter_analyzer.rs:1401`, `src/analyzer/tree_sitter_analyzer.rs:1414`).

`briefs/blind/python-filescope-enclosing-tie/brief.md:18`:
> - Java import recording calls the existing `push_hit` before reclassification (`src/analyzer/usages/java_graph/hits.rs:40`); `push_hit` increments the raw match count and updates the usage-limit flag from the collected set size (`src/analyzer/usages/java_graph/hits.rs:14`, `src/analyzer/usages/java_graph/hits.rs:35`).

`briefs/blind/python-filescope-enclosing-tie/brief.md:20`:
> - Rust import matching reads `direct_names` and `target_self_file` (`src/analyzer/usages/rust_graph/extractor.rs:248`); these originate from `usage_binding_names`, same-file seeds, reference-context bare names, and comparison with `target.source()` (`src/analyzer/usages/rust_graph/extractor.rs:134`, `src/analyzer/usages/rust_graph/extractor.rs:142`, `src/analyzer/usages/rust_graph/extractor.rs:148`).

`runs/python-filescope-enclosing-tie/BT/reviewers/a3c6477be8cd/output.md:1`:
> - [P2] Exclude import hits from the usage limit — src/analyzer/usages/java_graph/hits.rs:40-42 — When a Java target is imported in more files than the usage limit, these import hits set `limit_exceeded` before they are reclassified, so scanning stops and external usage queries can fail or omit actual references even though imports are filtered from their results. Scala has the same issue in `add_import_hit`.

`runs/python-filescope-enclosing-tie/BT/reviewers/5bc3eb2a4a49/output.md:1`:
> - [P2] Resolve each Rust use path before recording its identifiers — src/analyzer/usages/rust_graph/extractor.rs:247-250

## retained-fill-java-memo-loses-gap

Caught A1/3 to BT0/3.

Brief names definition_units/parent_units and nested-domain incompleteness propagation; it never names the Java class-index memo or its lost stable completion receipt.

OneBT handback diagnoses outer-failure contamination of nested fills, matching the propagation topic atbriefline10 and the recorded near-miss direction; the other2say No findings. The nonempty handback makes a concrete consequence claim, not a bare relation.

No findings contains no rationale; it is not evidence of stopping or trusting the brief. Tool transcripts unavailable.

The brief-to-handback topic overlap and absence of the particular Java receipt mechanism support a missing/competing-fact diagnostic, not a proven behavioral cause.

`briefs/blind/retained-fill-java-memo-loses-gap/brief.md:10`:
> - `inherit_query_incompleteness` compares child and parent domain IDs before propagating completion state (`crates/bifrost-analysis/src/analyzer/i_analyzer.rs:1495`); retained scope construction assigns the domain before `with_context` registers the context (`crates/bifrost-analysis/src/analyzer/i_analyzer.rs:3311`, `crates/bifrost-analysis/src/analyzer/i_analyzer.rs:3297`).

`briefs/blind/retained-fill-java-memo-loses-gap/brief.md:17`:
> - The selected domain supplies `definition_units` to SQL definition lookup and `parent_units` to parent lookup (`crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:13544`, `crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:10875`); the domain owner assigned at `crates/bifrost-analysis/src/analyzer/i_analyzer.rs:3311` therefore selects both memos.

`runs/retained-fill-java-memo-loses-gap/BT/reviewers/2047a53aabd1/output.md:1`:
> - [P1] Keep enclosing failures out of nested retained fills — crates/bifrost-analysis/src/analyzer/i_analyzer.rs:3311-3314. When a retained fill records an incomplete read and then starts another retained fill, both now have the same domain ID, so `inherit_query_incompleteness` copies the outer failure into the inner context. `replayable_query_cache_fill` consequently captures that unrelated failure in the inner entry's receipt; for a stable failure it can retain and replay it to later independent queries, causing clean derived results to be reported incomplete or rejected. Nested retained fills need failure isolation even when their lookup memos are shared.

`runs/retained-fill-java-memo-loses-gap/BT/reviewers/5721f3a6833f/output.md:1`:
> No findings.

`runs/retained-fill-java-memo-loses-gap/BT/reviewers/0c776de13cc6/output.md:1`:
> No findings.

## python-module-binding-representative-after-event

Caught A2/3 to BT1/3.

Brief gives event cutoff/ranges plus branch ancestry, but does not state that repeated bindings replace a shared CodeUnit with the later physical range.

OneBT says No findings; one reports class-local event-cutoff and deferred-lambda branch issues at sites the brief lists; one independently states the exact repeated-CodeUnit/range defect and catches it.

No explicit deference or stopping language; source-reading path unobserved.

Brief-site overlap is present, but aBT reviewer still supplied the missing range-replacement mechanism; treatment did not uniformly prevent its discovery.

`briefs/blind/python-module-binding-representative-after-event/brief.md:14`:
> - The timeline collector records functions and classes at their node end offsets, assignments at their node end offsets, and loop targets at the left-hand node end offset; these become the `event_visible_from` bound used to select the maximum eligible declaration end (`crates/bifrost-python/src/graph/extractor.rs:1632`, `crates/bifrost-python/src/graph/extractor.rs:1648`, `crates/bifrost-python/src/graph/extractor.rs:1660`, `crates/bifrost-analysis/src/analyzer/usages/get_definition/python.rs:1771`).

`briefs/blind/python-module-binding-representative-after-event/brief.md:16`:
> - Declaration end offsets and the AST lookup span in the new helper come from `analyzer.ranges(candidate)`; Python delegates ranges to its inner analyzer, which reads the unit's range vector from file state, including snapshot and current-source lookup routes (`crates/bifrost-analysis/src/analyzer/usages/get_definition/python.rs:1773`, `crates/bifrost-analysis/src/analyzer/usages/get_definition/python.rs:1784`, `crates/bifrost-analysis/src/analyzer/python/mod.rs:518`, `crates/bifrost-analysis/src/analyzer/tree_sitter_analyzer.rs:9642`).

`briefs/blind/python-module-binding-representative-after-event/brief.md:17`:
> - Timeline conditional flags recognize if, try, except, match, case, for, and while ancestry; the new branch-path comparison records if/elif arm ancestry, excluding condition expressions, and stops at module, function, or class boundaries (`crates/bifrost-python/src/graph/extractor.rs:1873`, `crates/bifrost-analysis/src/analyzer/usages/get_definition/python.rs:1799`, `crates/bifrost-analysis/src/analyzer/usages/get_definition/python.rs:1824`).

`runs/python-module-binding-representative-after-event/BT/reviewers/1f5de8767f84/output.md:1`:
> No findings.

`runs/python-module-binding-representative-after-event/BT/reviewers/c337a9bd5969/output.md:1`:
> - [P2] Preserve class-local bindings when selecting module events — crates/bifrost-analysis/src/analyzer/usages/get_definition/python.rs:1771-1775

`runs/python-module-binding-representative-after-event/BT/reviewers/c337a9bd5969/output.md:4`:
> - [P2] Stop branch ancestry at deferred lambda bodies — crates/bifrost-analysis/src/analyzer/usages/get_definition/python.rs:1810-1813

`runs/python-module-binding-representative-after-event/BT/reviewers/b8c775e9b5a9/output.md:1`:
> - [P1] Preserve earlier bindings when declarations share a code unit — crates/bifrost-analysis/src/analyzer/usages/get_definition/python.rs:1771-1778. For `x = 1; y = x; x = 2`, both assignments produce the same field code unit, and Python's declaration visitor uses `replace_code_unit`, so only the later assignment's range remains indexed. At `y = x`, that range ends after the first binding event, leaving `latest_end` empty and turning the previously resolved reference into `no_indexed_definition`. The same problem affects earlier occurrences of repeatedly defined functions or classes.

## php-indexed-write-reenters-current-rhs

Caught A2/3 to BT0/3.

Many brief items concern callback parameter/collection typing. Indexed-local inference is summarized as writes before the collection expression, without spelling out unfinished-write RHS re-entry.

All3BT handbacks report callback parameter reassignment/type-inference problems; none identifies the indexed RHS recursion. These add concrete overwritten-binding/type claims, not just copies of the neutral mapping relation. Baseline catches also contained callback findings, so the topic is not unique to the brief condition.

No explicit brief deference/stopping statement; tool transcripts unavailable.

Strong topic overlap with callback-heavy brief, consistent with diverted attention, but no direct process evidence of diversion or deference.

`briefs/blind/php-indexed-write-reenters-current-rhs/brief.md:17`:
> - The new callback helper finds the nearest local scope using the existing `is_local_scope` predicate (`crates/bifrost-php/src/graph/syntax.rs:465`, `crates/bifrost-php/src/graph/syntax.rs:31`); it maps the matching callback parameter's index to collection argument `index + 1` (`crates/bifrost-php/src/graph/syntax.rs:544`).

`briefs/blind/php-indexed-write-reenters-current-rhs/brief.md:18`:
> - `collection_element_type_fq_name` dispatches variable collections to indexed-local inference (`crates/bifrost-php/src/graph/syntax.rs:1538`, `crates/bifrost-php/src/graph/syntax.rs:1548`). That inference examines writes before the collection expression, skips nested local scopes, and returns an element type only when resolved element writes agree (`crates/bifrost-php/src/graph/syntax.rs:1400`, `crates/bifrost-php/src/graph/syntax.rs:1421`).

`runs/php-indexed-write-reenters-current-rhs/BT/reviewers/273929c3d88c/output.md:2`:
> - [P2] Stop using the original element type after callback reassignment — crates/bifrost-php/src/graph/syntax.rs:491-493. In a callback such as `function ($x) { $x = new Other(); $x->method(); }`, the helper matches every reference by parameter name without checking intervening writes, so the new branch resolves `method` against the collection element type instead of `Other`. This produces incorrect definitions and usage hits in both forward and graph resolution.

`runs/php-indexed-write-reenters-current-rhs/BT/reviewers/a551afaf2fdc/output.md:1`:
> - [P2] Stop using the collection type after a callback parameter is reassigned — crates/bifrost-php/src/graph/syntax.rs:491-498. In a block callback such as `function ($item) { $item = new Other(); return $item->run(); }`, this matches every later reference to `$item` solely by its parameter name. Both definition lookup and usage scanning then attribute `run` to the collection element type even though the receiver is `Other`, producing incorrect definitions and authoritative usage hits.

`runs/php-indexed-write-reenters-current-rhs/BT/reviewers/26aa1a6dc4bf/output.md:1`:
> - [P2] Respect callback parameter reassignment before inferring its receiver — crates/bifrost-php/src/graph/syntax.rs:1807-1819. When a block callback reassigns its parameter before a member access, such as `function ($item) { $item = new Other(); $item->run(); }`, this branch still assigns the collection element type to `$item` and bypasses the current local binding. Definition lookup and authoritative usage analysis can therefore attribute `run` to the wrong class; the bounded and unbounded forward paths make the same assumption.


## Equal-share comparison (not a loss)

Override-family is3/3withandwithoutbrief. The brief explicitlyjoins unproven/empty helper results withthe new empty-arm consumer, and allsixhandbacksidentifytheproof confusion. Itisnot a fifthnegativecase. This is descriptive text inspection; formalcontainslabelscome fromthe separately blinded assay.

`briefs/blind/unproven-override-family-certifies-empty/brief.md:14`:
> - `MemberFamilyAnswer::is_proven` compares the outcome with `Proven` (`crates/bifrost-analysis/src/analyzer/usages/member_family.rs:146`). The hierarchy helper collects `ImplementedBy`/`OverriddenBy` targets for that outcome, otherwise constructs an empty vector, and returns `Some(targets)` in both cases (`crates/bifrost-analysis/src/analyzer/semantic/workspace_oracle/dispatch.rs:945`). Both empty-vector routes enter the new `Some(overrides) if overrides.is_empty()` arm (`crates/bifrost-analysis/src/analyzer/semantic/workspace_oracle/dispatch.rs:554`).

`runs/unproven-override-family-certifies-empty/BT/reviewers/9f269ecf0c7a/output.md:1`:
> - [P1] Do not treat an unproven member family as an empty override set — crates/bifrost-analysis/src/analyzer/semantic/workspace_oracle/dispatch.rs:554-557

Competing handbacks make concrete failureclaims at brief-listed sites. This diagnostic does not verify those otherclaims or prove thebrief caused them; it cannot rule out relation-to-finding conversion. The absence of explicit deference language is not evidence that no deference occurred.
