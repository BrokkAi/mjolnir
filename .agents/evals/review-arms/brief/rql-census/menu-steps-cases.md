# RQL menu steps: worked cases, coverage ledger and adjudication rulings (scoring only)

This file goes with `menu-steps.md`, which holds the semantics. It holds census queries and their decisive rows, including some from pilot diffs, so it is used only in scoring and fidelity adjudication. It is NEVER given to the mock engine or to a writer: that would hand them gold answers. Frozen 2026-11-20 with `menu-steps.md`.

### Worked contract cases

Examples show the decisive fields in abbreviated form. Other qualifying cases and exact spans are still required.

1. **Unproven family returns a concrete empty vector.** Bifrost, `f5cf77da184060e453f69b0c4f04e27d1ba2ae8b`.
   ```rql
   (contract-cases :depth 1 :kind "return"
     (procedure-of (function :name "virtual_dispatch_implementor_targets")))
   ```
   Expected `return` case in `crates/bifrost-analysis/src/analyzer/semantic/workspace_oracle/dispatch.rs:970`: `outcome` carries `Some(targets)`, whose argument reaches the initializer alternative `Vec::new()` at line 958. `guards` includes the false outcome of `answer.is_proven()` (945), positive function test, no native entry, and successful provider lookup. The empty-vector fact does not require modeling the member-family provider. The caller's interpretation is outside the contract row.

2. **A stop reason drives the completion mapping.** Mjolnir, `a6674529c3117ef9b8ace7c02cd95487a1b6e372`.
   ```rql
   (contract-cases :depth 1 :kind "return"
     (procedure-of (function :name "classify_prompt_completion")))
   ```
   Expected `return` case in `mj-core/src/state.rs:201`: match `normalized.as_str()` selects the written pattern `"endturn"` and returns `PromptCompletion::Finished`. `dependencies` traces the subject to `stop_reason` through the char filter `*character != '_' && *character != '-'`, `char::to_lowercase` and `collect::<String>()` (195–200). The body contains no other input or completion check. Emit the other match arms too. The row need not evaluate a concrete Unicode string.

3. **A promptless archive propagates a specific failure.** Mjolnir, `6f26caa20f1e2277a240719da2276318649f3464`.
   ```rql
   (contract-cases :depth 1 :kind "error"
     (procedure-of (function :name "archived_session")))
   ```
   Expected `error` case in `mj-controller/src/sessionwiki.rs:1230`: after the initial loader calls succeed, the `snapshot_of(&session)?` edge propagates the failure of `!transcript.is_empty()` (1554), with message operand `"the archived session has no prompt to restore from"` (1555). The guard for that failure on loop exhaustion means no message passed both the trimmed-nonempty check and `Role::User` while `transcript` was empty. Show the empty initializer, skips at 1505 and 1510 and sole append at 1541. The current value of any session's messages remains symbolic.

4. **The span concatenation assertion is conditional on debug assertions.** Nushell, `aa7d7d0cc3bffcf4323291a7a4163e41dfbee6dd`.
   ```rql
   (contract-cases :depth 2 :kind "panic"
     (procedure-of (function :name "parse_let")))
   ```
   Expected `panic` case anchored at `crates/nu-protocol/src/span.rs:187`: the actual bound to `Span::concat`'s `spans` is `&spans[(span.0 + 1)..]` at `crates/nu-parser/src/parse_keywords.rs:3054`. The source conditions include `item == b"=" && spans.len() > (span.0 + 1) && span.0 > 1`, debug assertions enabled and some adjacent pair failing `a.start <= b.start && a.end <= b.end` (191). Depth two accounts for the callee and the `all` predicate closure. No prediction of the parser's actual token ordering is needed in this row.

5. **Empty.** At the completion-classifier revision above, `(contract-cases :depth 0 :kind "callback" (procedure-of (function :name "classify_prompt_completion")))` emits zero callback cases. The seed has a string formal and invokes no callable formal or alias of one. Its own iterator invocations do not meet the selector.

### Worked value transfers

1. **Launch directory comes from the selected session field.** Mjolnir, `e9929f997ccdafcc07a99b1a82d3af1247c5efc0`.
   ```rql
   (value-transfers :direction "upstream" :port "return" :depth 0
     (procedure-of (function :name "worker_launch_config")))
   ```
   Expected row in `mj-controller/src/hel_controller/worker_binary.rs`: source `&session.project_directory` (332), under the successful `Some(project_directory)` pattern. Path binds `project_directory`, transforms it via `project_directory.to_string_lossy().into_owned()` (333), constructs `workspace` tuple component 0, reads `&workspace.0` and transforms it via `PathBuf::from(&workspace.0)` (389). Sink is the returned `Ok` tuple's component 0, `WorkerLaunchConfig.cwd`, whose written initializer is `PathBuf::from(&workspace.0)`. State that this is the project-directory branch. The row does not need to infer the export directory or any actual file write.

2. **Clean status and report travel separately into the same aggregate.** Bifrost, `a830471d19390c947b93e047483ab4e41f49ebe5`.
   ```rql
   (value-transfers :direction "upstream" :port "argument" :depth 1
     (procedure-of (call :name "structured_only")))
   ```
   The call match seeds the immediately invoked closure owning this call. The decisive row's source is `"clean"` (7026), with guard `POLICY_EXIT_CLEAN` matched against `exit_status`, via `outcome.exit_status()` (7024) returning `self.exit_status` (`crates/bifrost-policy/src/coordinator.rs:621`). Sink is the `status` field of the written `RunPolicyToolResult` actual. The aggregate records sibling `report: outcome.into_report()` (7039) whose result originates in `self.report` (`coordinator.rs:513`, separate sibling transfer also emitted). The selected match arm has no predicate on report completion. `structured_only` is the call-argument boundary for this query. Return transport through `snapshot.finish` is outside its scope.

3. **A worker lookup result supplies the copy source.** Mjolnir, `29310d2d795305389d96a50b60e1c2705d03c4cf`.
   ```rql
   (value-transfers :direction "downstream" :port "return" :depth 4
     (procedure-of (function :name "worker_binary_for_arch")))
   ```
   Expected row in `mj-controller/src/hel_controller/worker_binary.rs`: source port is the `Ok` payload's `WorkerBinaryAvailability::Local.path` component of the seed return `snapshot.resolve(arch, requirement)` (1014). The success edge of `?` and the `WorkerBinaryAvailability::Local { path, .. }` match at 1191–1192 establish that component on this route. The binding moves into `Ok(path)` (1192). The next caller receives `worker_binary_for(backend, executor)?` into `worker_binary` (131), passes `&worker_binary` (139), and the `LocalBare` branch uses `worker_binary.to_string_lossy().into_owned()` (1615) as the first argument operand after the program `"cp"` (1613). The sink port is the array element passed to `CommandSpec::new`, so the row need not assume a successful copy or a particular runtime pathname.

4. **A successful suffix fast path yields the EndWith result shape.** Rspack, `238cda812314b7e506b0b9c212b29eb0eb13640d`.
   ```rql
   (value-transfers :direction "upstream" :port "return" :depth 0
     (procedure-of (callable :name "try_compile_to_end_with_fast_path")))
   ```
   Expected row at `crates/rspack_regex/src/algo.rs:85`: the reached constructor operand is exactly `Some(Algo::EndWith { pats })`. Its guards include successful `regex_syntax::parse(expr).ok()?` (74), the true outcome of `is_ends_with_regex(&hir) && seq.is_exact()` (78), and success of `.literals()?` (80). Keep `pats` as an opaque written collection result if its callback is beyond depth. The existing test at 146 confirms realizability for a concrete pattern. The query need not run or consult the test to prove the conditional constructor alternative.

5. **Empty.** At the worker-lookup revision, `(value-transfers :direction "downstream" :port "return" :depth 0 (procedure-of (function :name "worker_binary_for_arch")))` emits zero consumers. The selected port is a return operand, and the first forward edge out of that port is to a caller result outside depth. It does not emit the seed's reaching definitions or a zero-length self-route.

### Worked guarded paths

1. **The CSS load is gated by the cached state and matcher.** Rspack, `b76f68eeecfbf258592919c40f1f926cc50c6b0d`.
   ```rql
   (guarded-paths :event "call" :depth 0 (call :name "loadStylesheet"))
   ```
   Expected event at `crates/rspack_plugin_runtime/src/runtime_module/runtime/css_loading_with_loading.js:51`, `loadStylesheet(chunkId, url, loadingEnded)`. Guards select true at `installedChunkData !== 0` (6), false for the `installedChunkData` test (10), and true for `CSS_MATCHER` (13). The tested version is the value defined at lines 3–5 by either `installedChunks[chunkId]` (when `__webpack_require__.o(installedChunks, chunkId)` is true) or `undefined`. Emit separate alternatives with those corresponding guards and preceding normal-completion edges. A proven value of 0 at the *test* bypasses the call; the later callback write at 16 is not the tested version. This query works on the authored template without expanding `CSS_MATCHER` or asserting the runtime state of any particular chunk.

2. **Restoring contents can target the archived commit.** Mjolnir, `86494146fd9e2c0fc03b1a2422bc2f952a2e4a3a`.
   ```rql
   (guarded-paths :event "call" :depth 0
     (procedure-of (function :name "restore_git_snapshot_for_session")))
   ```
   Expected event at `mj-checkpoint/src/archive/git.rs:2017–2024`. Guards include `restore_contents`, the true outcome of the short-circuit operand `snapshot.committed_bundle.is_empty()` (2004), `Some(branch)` from `&snapshot.metadata.branch` (2009) and successful return from the ref-check call at 2010–2016. The command operand is exactly `["checkout", "-B", branch, checkout_target]` (2020), with `checkout_target` originating from `snapshot.metadata.head_commit.as_str()` (2005). The ref-check is an ordered predecessor. A separate case covers branch absence. Emit the branch-absent case at 2026–2033 with `["checkout", "--detach", checkout_target]` as well. Include the additional `clone_refs` and `"FETCH_HEAD"` alternatives in the full answer.

3. **Cancel writes the List view through the reset helper.** Mjolnir, `981532a239c40960a673b7f10a369082d666c57b`.
   ```rql
   (guarded-paths :event "write" :depth 1
     (procedure-of (callable :name "handle_workspace_manager_event")))
   ```
   Expected event at `mj-tui/src/workspaces.rs:280`, place `self.view` on the receiver bound from `manager` and written RHS `WorkspaceManagerView::List`. The incoming `manager.reset_to_list()` call (740) follows a `Mode::WorkspaceManager(manager)` match, `Some(Interaction::Cancel)` with earlier Close case excluded, false `locked` and false `matches!(manager.view, WorkspaceManagerView::List)` (719–740). Show the definition of `locked` at 733–735. The closed guard dependency set for the route includes mode, action, busy state and view. The widget's production of the action remains symbolic.

4. **The caller still holds the guard across the nested call.** Mjolnir, `7b6948038500a5883dc6edb0e96b11176fb49d31`.
   ```rql
   (guarded-paths :direction "incoming" :event "call" :depth 1
     (procedure-of (callable :name "active_lifecycles")))
   ```
   Expected pivot at `mj-controller/src/daemon.rs:2095` in `session_projection`. A path row for the callee's `.lock()` (1999) has prior acquisition from `self.controller` at 2091–2094, same `self` receiver/field identity (114), and the live local guard `controller` through the synchronous call. Its later use at 2096 proves the binding still lives at the pivot. Treat the first acquisition and poison-recovery wrapper as successfully supplying a guard on this case. State the second operation as a call attempt, independently of whether it returns.

5. **Empty.** At `a6674529c3117ef9b8ace7c02cd95487a1b6e372`, `(guarded-paths :event "write" :depth 0 (procedure-of (function :name "classify_prompt_completion")))` emits zero write events. The `normalized` initializer is a declaration, and the source body has no assignment to an already bound place, field or index. Built-in string collection is a value-producing primitive, and its undisplayed implementation is outside this query.

## Proposal coverage ledger

**Covered** means the stated source relation fits this draft after rewriting the proposed surface syntax. **Partial** means the step can emit a concrete part of the claim. The full fact requires a stronger relation or a different representation. **Outside** means the proposed decisive relation crosses a boundary of the draft. These are scope assessments from the current labels and cited source, before candidate retrieval or mock runs. Candidate retrieval and mock fidelity remain unmeasured. The ledger classifies 8/10 contract, 5/11 value-flow and 7/11 guarded-path bugs credited by the menu as having their proposed facts in scope. ★ marks a fact in a bug credited to that kind by the menu. Re-run the menu using verified decisive rows before comparing these definitions to its development counts. Bug IDs below refer to the current primary label.

### Contract

| Proposal | Scope under `contract-cases` |
|---|---|
| ★ `jvm-protected-unindexed-hierarchy-denied` | Covered for the empty-edge case. The queue starts with one owner, and accepted empty edges cause no enqueue or uncertainty mutation. The exhausted path evaluates `Ok((!uncertain).then_some(false))` with `uncertain` still false. This does not require knowing whether the hierarchy actually is complete. The builtin `then_some` selects the Option variant using the boolean. |
| ★ `php-indexed-write-reenters-current-rhs` | Covered for the callback invoked on a visited assignment: its start-byte and nested-scope filters, matching indexed LHS and exact `right` actual. The guards allow the assignment to encompass the supplied offset. This scope does not claim that the unknown callback recursively reenters the resolver. |
| ★ `unproven-override-family-certifies-empty` | Covered by the unproven branch and concrete empty constructor in example 1. |
| ★ `formatter-string-strips-escapes-without-quote-switch`, quote selection | Covered symbolically by independent counts per visited char, alternate on strict majority, preferred on tie. The row's recurrence includes the fact that every quote char is counted, whatever its predecessor. No test-string-evaluation feature. |
| ★ Same Mago bug, string rewrite | Covered symbolically by the iterator transition on a backslash, the peek/consume of the next char and the output appends conditional on `next_char != other_quote`, with the other quote derived from `enclosing_quote`. Model initial/final delimiter appends. The row states the transformation for that class of inputs without supplying a synthetic string. |
| ★ `archive-resume-hint-without-prompt` | Covered by the error and propagation in example 3. |
| ★ `handback-reminder-stops-work` | Covered by the classifier mapping and its closed local guard dependencies in example 2. |
| ★ `journal-valid-json-corruption` | Partial. A mismatch of stored versus recomputed digest leads to the written rejection. The row can show payload construction and inputs to serialization/hash. It cannot certify an integrity contract across external serde implementations or hashing, nor derive what all serialized inputs protect. |
| ★ `dev-alias-completion-indexes-alias-offset-out-of-bounds` | Covered for a finite witness: the call-argument branch and an explicitly selected nested binary/list constructor can contribute multiple entries. Report recursive composition symbolically. A global cardinality bound over arbitrary AST recursion and the caller's eventual panic are outside the contract. |
| ★ `dev-detect-columns-combine-columns-range-bug` | Covered for signed endpoint preservation and the fallback when the conversion or range type takes a different case. The seed's interface and outcome use no collection length. The caller's cast and slice are separate facts. |
| ★ `rust-lang__rust-analyzer-dev-012` | Partial. The parser can emit an error and still complete `BIND_PAT` without completing `NAME`. Joining a constructed syntax node to a later AST accessor on that node and proving its `None` result requires an independently resolved use/producer relation. |
| `cpp-template-inherited-base-signature` | Covered for the signature-over-source choice and colon-free empty-return alternative. The template identity writer is a separate fact. |
| `go-nilness-ignores-modeled-nonreturn` | Covered where the unique-summary and control predicates resolve in the bounded slice. Keep opaque program model results symbolic. |
| `js-synthetic-owner-leaks-file-summary` | Covered for the returned field mapping and the conditional stored-range calculation through the iterator closure, including both written operands of the maximum. The actual stored range is a separate input. |
| `kotlin-source-constructor-steals-class-header-comment` | Outside as proposed. Registration, subsequent lookup in a shared index, and comparison across two declaration walkers need an index identity/eligibility model. Local guard cases can expose synthetic exclusion or depth increment at individual sites. |
| `projection-mixed-sqlite-snapshots` | Covered where the validator call and local iterator borrow resolve. Map `through` back to the session frontier and emit the assertion/error case. |
| `dev-lite-command-redirection-spans-unsorted-panic` | Covered by the debug assertion case and actual binding in example 4. The token flow that makes the range unsorted is a separate fact. |

### Value flow

| Proposal | Scope under `value-transfers` |
|---|---|
| ★ `dependency-failure-drops-independent-model-evidence` | Partial. It can trace request evidence into actuals and local selection/constructor ports. Certifying which models an open catalog selects, and independence from dependency discovery as a whole, exceeds positive local flow evidence. |
| ★ `kotlin-value-class-this-loses-underlying-projection` | Partial. Receiver creation, exact receiver fields and the captured/fallback reads are in scope when the local receiver identity is proven. Absence of a carrier-map association across the lowering passes is a keyed state or effect proof. |
| ★ `mcp-default-advisory-reported-clean` | Covered by the literal-to-field route plus aggregate report operand in example 2. No coordinator behavior needs to be inferred. |
| ★ `rust-option-return-implicitly-unwrapped` | Covered for signature origin, parsed-return helper and its conditional propagation to the selected result. No proof of syntactic unwrapping can be inferred from a type string. A closed guard slice can show the absence of such a gate in the helper. |
| ★ `child-model-wrong-efforts` | Outside as proposed. The full provenance crosses cache entries, external worker transport, and an event-driven probe result. A larger call depth would still leave those boundaries open. |
| ★ `export-relative-read-root` | Covered for the `cwd` field provenance in example 1. It stops short of observing where a worker writes. |
| ★ `github-token-in-container-argv` | Partial. Local copies, field writes and exact consumer actuals are supported, as is a terminal process-builder operand. Connecting independently used configuration/template instances requires proof of the same mutable object. A same-name field join earns no credit. |
| ★ `pinned-worker-disappears` | Covered for the downstream copy operand in example 3. The stale-path condition is an external circumstance. |
| ★ `rust-lang__rust-analyzer-dev-067` | Partial. `ExprStmt` append, the returned statement port and the conditional tail-pop/push are positive flow. The proposal additionally needs an explicit omission case for `Stmt::Item` across the collection and a realized full input block; a missing path is no substitute. |
| ★ `rust-lang__rust-analyzer-dev-094` | Partial. Constructor/field trees and distinct variant origins can be shown for each producer separately. The combined derived equality argument, generic instantiation across open builder internals and concrete type-pair witness are outside this transfer relation. |
| ★ `rspack-dev-regex-endwith-global-panics` | Covered for the conditional constructible result in example 4. Concrete regex parser evaluation and the test-to-callee relation are out. |
| `cpp-template-inherited-base-signature` | Covered for the exact slice/formatting expressions defining the constructor actuals and their distinct template identity versus presentation ports. Unknown text remains symbolic. |
| `catalog-cache-key-collision` | Covered for the `None` selector reaching the written default string and the fingerprint reaching the cache-save actuals. No persistent cache readback is needed to show those call inputs. |
| `dev-known-external-uses-unprefixed-decl-name-for-module-exports`, command words | Partial. A transfer can carry the exact first index and remaining word-derived actuals through locally resolved builder calls. Runtime command lookup and unproved dynamic `command.run` dispatch stay open. |
| Same Nushell bug, stored name | Partial. The initial name field can be traced in the constructor and its local store; reconnecting that allocation to a detached declaration-slot lookup and trait accessor needs a proven object identity beyond those calls. |
| `rust-lang__rust-analyzer-dev-098` | Partial. Direct construction and the call operands to the fold/normalization sites fit. Proving preservation of all nested BoundVars through arbitrary recursion and fold callbacks exceeds bounded source flow. |
| `rspack-dev-eval-logical-sets-bool-type` | Outside as proposed. No `:expression` free-text input, synthetic AST or execution of the evaluator. A symbolic trace for authored literal arms can still state their field assignments. |

### Guarded path

| Proposal | Scope under `guarded-paths` |
|---|---|
| ★ `incompatible-semantic-bundle-late-gate` | Covered for the earlier checksum/read/hash/decode call sites and curated iteration on the route to the rejected production branch, conditioned on successful earlier checks. Count checked-in helper calls, not the internal steps of OS I/O and hash functions. |
| ★ `rust-bare-value-admits-associated-constant` | Covered for reaches-the-filter-acceptance case and candidate/parent identity tests, with the written enclosing-owner iteration represented symbolically. No proof of graph completeness or of a particular repository's candidate set is needed for the guard. |
| ★ `rust-item-macro-evades-plain-struct-drop-proof` | Covered for the `any` closure and guard around the written omission operation. Exact incoming identity and the collection-wide all-false case show the skipped branch. This concerns analyzer source, with no macro-expansion or runtime Drop inference. |
| ★ `held-prompt-ignores-interrupt` | Partial. The validator rejection and its placement before dispatch are source relations. Joining asynchronously published capability snapshots to pending-work ownership at the same runtime instant, or proving no path ever cancels it, is outside. |
| ★ `metadata-checkpoint-overwrites-checkout` | Covered by the branch and command-operand relation in example 2. |
| ★ `reserved-workspace-internal-error` | Partial. Typed conversion and status selection can be observed in their local procedures. The full end-to-end propagation crosses the writer lane and awaited task result; the queued execution link is outside. |
| ★ `shortcut-escape-opens-unvisited-manager` | Covered by the guarded view write in example 3. |
| ★ `web-feed-self-deadlock` | Covered by the exact same-place guard and nested acquisition attempt in example 4. No lock runtime result is predicted. |
| ★ `coreutils-dev-mv-self-move-aborts-earlier-sources` | Partial. Ordered iteration, basename skip and written rename operand are in scope. The per-error continuation hinges on `show_if_err!` and `show!`, which are outside the written macro registry. |
| ★ `rspack-dev-css-loading-initial-chunks` | Covered for the local template guard in example 1. The step does not run template substitution or show which runtime chunks were prefilled. |
| ★ `rspack-dev-module-import-external-panics-none` | Partial. The object-map match, forwarding actual and fallback type are in scope on resolved paths. Eligibility as a `ModuleDependency` does not establish an actual concrete runtime `CommonJsRequireDependency` instance flowing through unresolved trait/provider calls. |
| `go-nilness-ignores-modeled-nonreturn` | Covered for the guard of the successor scheduling event. Distinguish the local skip from the earlier operand updates. |
| `restore-through-profile-symlink` | Partial. Local removal and installation operations can be ordered in a resolved body. Lifetime order through joined concurrent lanes and the later restore effect is outside the defined execution relation. The label is counterevidence. |
| `dev-lite-command-redirection-spans-unsorted-panic` | Partial. Show redirection and ordinary-item events and guards per loop iteration. A full token-stream witness through arbitrary state transitions and the separate parser's input is outside. |
| `rust-lang__rust-analyzer-dev-098` | Partial. A source-proven callback trace can order registration, canonicalization and return. Proving preservation of nested opaque substitutions through recursive fold/dispatch requires additional value semantics. |
| `rspack-dev-banner-plugin-drops-sourcemaps` | Outside as proposed. Hook registration, macro-expanded stage dispatch and same asset identity across plugin callbacks cannot be joined. Source-written stage order and the consuming plugin's local map guard can be queried separately. |
| `rspack-dev-eval-logical-sets-bool-type` | Partial. A resolved incoming route may show the type and side-effect guards and replacement constructor operands. The concrete nested expression trigger and evaluator recursion remain symbolic/open. |

## Fidelity rulings to freeze before implementation

| Ambiguity | Proposed ruling |
|---|---|
| Which path counts when a callee result is opaque? | Credit the conditional source relation down to the written opaque port. An answer claiming a computed value or an exhaustive set of runtime outcomes beyond that port is unsupported. For case comparison, guard conditions must be equivalent under exact trace facts. Omitting an early-exit exclusion that changes membership makes the row wrong. |
| Equivalent syntax or reordered guards | A canonical semantic key compares source spans, typed guard outcomes and endpoints. Preserve evaluations' execution order, especially short-circuit and side-effectful predicates. Do not compare a rendered English paraphrase or compare literal values by decoded equality in place of exact evidence. Equivalent explicit/implicit propagation may use the same typed operation tag with distinct site evidence. |
| Negative conclusions and completeness | Require the relevant path/guard/write domain to be closed for “no check,” “no write,” “never reaches” or “no normalization.” At a depth boundary state the claim only within the bound. A conditional branch outcome remains a positive result when another branch is open. |
| Loops and recursive paths | Grade the source-backed recurrence and the guarded exit/continue/break, including zero iterations. Credit a finite constructor or call witness. Require an explicit closed invariant for “all” and loop-exhaustion equivalences. Repeated recursive invocations charge depth; repeated iterations summarize from one source body. |
| Same-valued expressions or shared field names | Require lexical/projection identity and invocation binding. Text equality is never a substitute for object identity. Mark unresolved aliases open and keep known alternatives. `#[derive(PartialEq)]` is not a written comparison execution path. |
| Auxiliary rows and display budget | A proof path may contain multiple evidence sites. For the location arm keep those sites together and strip values, guards, endpoint descriptions and prose as the plan requires. Gold enumerates the complete semantic rows first. Truncating a display or matching a cited location alone does not establish a decisive fact. |
| Counting mixed fact claims | Record stated-in-row independently from location coverage. Credit a full fact only where a single row's typed relation states it or logically entails it with its own guard/operand fields. A missing runtime conclusion that the reviewer can draw with the diff is a separate question. A “partial” ledger entry earns no full-row credit until the missing conjunct is demonstrated. |
| Unsupported language, external invocation, or absent seed | Unsupported input or model capability gets a coverage diagnostic. A well-typed query can return complete empty on a closed source domain with no eligible rows, including no seeds. External declarations are excluded from the seed set. An external call written inside a seed may be an endpoint with opaque behavior. |

Validation used the census's syntax and row-composition checker with the three proposed signatures registered in memory. All 15 worked queries, including the three empty cases, passed that check. The pinned engine has no implementation of these draft semantics.

Open decisions for the coordinator:

1. Adopt the conservative source-local boundaries and measure the resulting loss of menu coverage before freezing, or deliberately add a separately specified semantic model for writer lanes, shared allocation identity or generated dispatch. A deeper `:depth` alone does not close these gaps.
2. Confirm that typed single-site classification plus deterministic cross-site composition is the intended mechanism for contract and guarded paths. If the mock will classify whole traces, the preregistered candidate-classification and fidelity unit descriptions need an explicit amendment.
3. Decide whether a closed symbolic case such as the Mago character recurrence or Rspack suffix constructor earns decisive-row credit without a concretely evaluated fixture, when the case logically entails the labeller's example. This draft proposes credit for the relation and keeps runtime claims separately testable.
4. Freeze exact option encoding and row projection fields after checking the exemplar queries against the typed mock registry. The pinned RQL engine intentionally lacks these operators.
