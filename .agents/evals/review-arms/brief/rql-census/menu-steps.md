# RQL menu steps for the Stage 2 lean pilot (frozen 2026-11-20)

These are the three added steps, `contract-cases`, `value-transfers` and `guarded-paths`, one for each menu kind (contract, value_flow, guarded_path). The mock engine answers by these semantics, and fidelity is judged against them. They were drafted from the 51 current labeller proposals of the three kinds. The parent's rulings on the draft's open decisions come first and govern where they differ from the draft text below.

## Rulings (dbcece95, 2026-11-20)

1. Scope: the conservative source-local boundaries stand. No separate semantic model is added for writer lanes, shared allocation identity or generated dispatch. The menu coverage these steps keep (8 of 10 contract, 5 of 11 value-flow and 7 of 11 guarded-path menu bugs fully in scope; see the coverage ledger) is reported in the readout. A fact outside a step's scope counts as not expressible by that step.
2. Mechanism: in the lean pilot, the engine answers each query whole. One Fable/high agent reads the post-change tree under these semantics and returns the complete rows, rather than the plan's per-candidate classification with deterministic cross-site composition. That method would need a static analyzer the pilot can't build in time. The study plan's Lean pilot section records this amendment. The prescience check (decisive-row recall within 10 points of other-row recall) and the precision and recall bars on the fidelity sample are the safeguards. Where the text below describes local classification and composition, it describes the full design, not the pilot engine.
3. Credit: a row earns decisive credit when its own fields state the fact or logically entail it, including a closed symbolic case that entails the labeller's concrete example. A runtime conclusion that needs the diff or other code stays a separate question. A "partial" ledger entry earns no full-row credit.
4. Encoding: the exact option encoding and row projection fields are frozen by the engine slice, after checking every worked query below against the mock's typed registry. The worked queries are the reference; any change is recorded in this file before any writer runs.

---

# Stage 2 draft for the three added RQL steps

This draft defines the answerable relations before any Stage 2 answers are generated. It uses the pinned reference's nested query syntax. The names and depth limits are proposed for the mock and do not describe capabilities of the pinned Bifrost engine.

I reproduced the menu's current-label selection with `item_sets`, `labeller_item`, `checkout_revision`, `inputs_sha` and `read_current_labels`. It yielded 96 current primary labels and skipped 130 stale rows. Each selected kind had 17 outside missing fact proposals. `decided_kinds`, applied to confirmed relation bugs, credits 11 of those facts for each kind, in 10 contract bugs, 11 value-flow bugs and 11 guarded-path bugs. The proposal ledger below covers all 51 facts. The counts describe the original menu, before these scope limits. All example revisions are the introducing commits.

| Census kind | Step and JSON op | Input → output | Question the row answers |
|---|---|---|---|
| `contract` | `contract-cases`, `contract_cases` | `procedure → contract_case` | What does the callable return, reject, or pass to a callback under stated conditions? |
| `value_flow` | `value-transfers`, `value_transfers` | `procedure → value_transfer` | Which written value flows into or out of a selected interface, and through which transfers? |
| `guarded_path` | `guarded-paths`, `guarded_paths` | `procedure` or `call_site` or a call `structural_match → guarded_path` | Under which branches does an operation execute, and what happens before it on that route? |

One step per kind. In particular, normal results, failure conditions and callback invocations use the same contract-case structure. Forward and backward value provenance reverse the search over the same edges. Guarded reachability and order use the same route, with order derived from its event sequence. Separate implementation modes would duplicate semantics. Some proposals use both a value and a guard, or call their output a return case. Classify by the assertion in the row: whole-call outcome, value transfer between ports, or occurrence/order of an operation.

Existing `call_result_contract` denotes a protected acquisition and has consumers that assume that meaning. Existing `flow_endpoint` and `flow_witness` are tied to registered plans and retained bounded evidence. Existing `flow_relation` identifies events without their values, and `branch_relation` compares arms within one structural match. None can be reused as-is for these relations. Reuse existing source spans, declaration/procedure identities and typed call and expression identities where available. Each step adds exactly one row kind. The outer form takes exactly one nested row-producing query, with options before it. Selector values shown as quoted strings are closed enums and extra or repeated options are errors. Each new kind can feed `filter`, `project` and `file-of`; for `file-of` the primary anchor is the outcome, sink, or event defined below. None implicitly coerces back to a `procedure`. Authors can turn a declaration or structural callable match into a procedure with `procedure-of`. Path restrictions can wrap the seed before the added step. When a name matches several seeds, evaluate each; a method name alone supplies no receiver identity.

## Common answer rules

- Use authored workspace source at the introducing tree. Rust and authored JavaScript are in scope, including checked-in JS templates treated as written. Traverse conditionally compiled Rust definitions under their written configuration predicate. Treat configuration as unbound unless established by source within the query. Do not read generated artifacts, expand arbitrary macros, install dependencies, run code, synthesize AST inputs or use later history. The Rust macro cases below are recognized only at written invocations with resolved primitive identity.
- A site has repo-relative path, byte range, line span and exact source text. Every reported written operand, field initializer, literal, predicate and command argument carries its own site. Preserve spelling, quotes, escapes, integer base and internal whitespace. Copy a single-line operand verbatim from its line. For a multiline operand carry exact per-line slices and ranges so the same check applies. Separate typed derived relations (negation, identity, a wrapper, literal kind, zero elements, a composed port) from written values. For example `Some(targets)` and `Vec::new()` are two source snippets linked by a reaching-definition relation. Do not report a synthesized `Some(Vec::new())` as written.
- A guard is an ordered conjunction of typed atoms (source-test site, selected outcome and reaching definition or formal binding where available). Match arms include pattern bindings and earlier arm exclusion; capture short-circuit order and success edges of `?` and analogous written early exits. Use separate cases for disjunctions. An opaque boolean or return is a symbolic test or operand. Such an atom permits a conditional statement about the surrounding code. It cannot prove that the condition occurs in practice. Drop a route as infeasible only with a proof in the slice, such as incompatible variants of the same unchanged binding. No product-intent assumptions.
- Resolve lexical bindings and fields by identity, never spelling alone. Bind actuals to exact formals, including receivers. Traverse a uniquely resolved inherent method, concrete trait override or default body selected for a proven receiver. For multiple known targets, report a separate proof per explicitly resolved target, conditioned on receiver identity. Unresolved dispatch leaves an open frontier. Trait bounds and `impl` declarations alone establish type eligibility, not dispatch or a live runtime instance. A syntactically visible callback invocation can be an endpoint even when its body is open.
- Preserve an `open` frontier when a missing identity, ambiguous mutation, unsupported transformation or unknown control effect could change eligible rows. Coverage is per seed and query options, with a reason and source site for each such frontier. An opaque written value can terminate a complete relation to that value while an investigation of its origin remains open. Each depth boundary is an explicit query boundary, not an inference about deeper code. Return the call or value port at the boundary if it qualifies, with a depth-boundary marker. Unresolved transitions *inside* the scope are open, never evidence that nothing happened. Propagate upstream coverage separately from rows, including when the result is empty or a terminal filter removes every row.
- Sort by primary source span, seed identity, path/guard sequence and the remaining endpoint identities. Deduplicate only equal semantic keys (source identity, invocation path, selected outcomes, endpoint and row-specific relation). Distinct writes, calls, conditions and expansions of one reusable body at different call sites remain distinct. Render structured fields and evidence without summaries of defects, repairs or consequences outside the step. A row is decisive only if its relation fields state the fact. A row pointing at a function or holding only an opaque reference to its result does not earn credit for that function's behavior.
- Define completeness before display. For resolved, modeled inputs return every qualifying case in the bounded semantics. This includes uninteresting rows and proven rows from partially open answers. A complete empty answer requires that the eligible domain is closed and contains no rows. Display limits, candidate-source misses, classifier mistakes and read failures do not change the gold relation. Record candidate-source tags per sighting and hop. Candidate enumeration uses the plan's union. Local classification produces typed source facts one candidate at a time. The engine joins those facts by exact identities and traversal rules for multi-site rows. A whole-route classification with access to every candidate would need a change to the plan.

### Source-local operations used by the three steps

This is the proposed small built-in model shared by the steps. Resolve the language or standard-library identity before assigning its semantics. An identically named workspace routine is analyzed as ordinary source.

| Construct | Rule |
|---|---|
| Bindings, tuples, structs, enum constructors, arrays and member/index projections | Transfer written operands independently. Destructuring binds the selected fields/components. For indices require an identical proven key or an exact fixed ordinal. Split at joins by reaching definition; a local compound update reads the old binding and writes a transformed value, and a later must-write kills the earlier value for that location. Include ordinary expressions as written transformations with ordered operands. Apply builtin boolean/variant tests and scalar comparisons only where the types and operands establish them. |
| `Option` and `Result` | Recognize written constructors, match/let-else, `?`, `ok`, `map`, `and_then`, `or_else`, `is_some`, `is_none`, `is_some_and`, `is_none_or`, `unwrap_or`, `unwrap_or_default`, boolean `then`/`then_some` (lazy/eager respectively), and Rust `From` on an error only where its target body is resolved. Record the callback and selected port. An unknown custom default or conversion stays symbolic. |
| Owned local collections and strings | Model written empty initializers, `len`, `is_empty`, `push`, `pop`, `extend`, `VecDeque::from` on a fixed array and its `pop_front`/`push_back`, fixed indices, and ordered readonly iteration. A resolved standard sort preserves the element count and opens per-element order unless its keys are established. For Rust standard `iter`, `into_iter`, `enumerate`, `windows`, `chars`, `peekable`, `next`, `peek`, `filter`, `map`, `flat_map`, `for_each`, `any`, `all`, `collect`, describe symbolic visits, emitted/omitted elements and a per-iteration update. `filter` closure returns are inclusion guards. Keep short-circuiting and lazy evaluation until a consumer invokes the closure. Represent loop-carried state by a source-backed recurrence with initial, step and exit cases; derive an exhaustion invariant only for a monotonic property with all updates accounted for, such as no append until the first qualifying element. No unbounded symbolic search, traversal of arbitrary graph reachability or evaluation on supplied fixture strings. |
| Conversions and opaque calls | Track builtin signed conversion success/failure when its representability condition is available, and primitive path/string/ref conversions and `str::trim` as symbolic transformations with written inputs. Separate the resulting value or effect from its arguments. Unmodeled library calls can produce an opaque result conditioned on normal completion, and can still be call endpoints. A direct input binding is provable even when nothing about the call result is known. |
| Closures, borrow and await | A closure body is executable only at a resolved source-local invocation or a listed synchronous primitive (including consumption by `collect::<String>()`). Bind its captures and arguments by identity and charge its invocation as a call edge. A nonescaping mutable borrow of a known local or exact field can be tracked into a resolved callee and back. Await a resolved directly constructed future along its own evaluation order. An arbitrary executor, detached task, callback registration, yield to another task, queue, IPC, or shared mutable heap is outside the traversal. Record known order of written calls before those boundaries. |
| Written macros | Model source-backed Rust `assert!`, `debug_assert!`, `panic!`, `unreachable!`, `anyhow::ensure!`, `anyhow::bail!`, and `vec!` with written operands. Guard `debug_assert!` on debug assertions enabled. Treat `format!` as a symbolic formatted value with its written template and operands, and `matches!` as a pattern test. No procedural macros, implicit synthesized field or method bodies, arbitrary expansion, inferred logging-macro effects, implicit panics from indexing/arithmetic, or general destructor behavior. Guarded paths can track the lexical lifetime of a resolved Rust std `MutexGuard` from acquisition to explicit drop, move or scope exit when the guard stays local. |

### Depth and counting

`:depth` is an integer. Depth zero uses the seed procedure body only. A justification can include several chains from the root, such as one for a predicate and one for an operand. Limit each chain individually, counting one edge to enter a different callable invocation. Separate sibling calls each start from the same caller depth. Return, unwind and mutable-borrow result back over that invocation do not charge twice. A resolved callback body counts even when written inside the same file. A modeled collection primitive costs no call crossing for the primitive itself; invoking a callback inside it does. A recursive call counts like any other invocation, with repetition representing newly instantiated calls. An ordered loop recurrence is not a call. Exclude external or unresolved bodies, preserving written endpoint evidence. Traverse source-local assignments and expressions without a statement or hop-display cap. Bounds constrain which cross-body facts the query claims, not how many found rows to show.

## `contract-cases`

```text
(contract-cases :depth 1 :kind "return" (procedure-of (callable :name "target")))
```

`:depth` defaults to 1 and accepts 0–4. `:kind` selects `"return"`, `"error"`, `"panic"`, `"callback"` or `"all"` (default). Four covers the finite helper depths named by the contract proposals. Deeper semantic claims in the proposals involve recurrence or serialization through workspace boundaries and merit explicit checking before expanding support. A panic is a modeled explicit failure, not implicit runtime failure. An error is a `Result::Err` (including propagation by `?`), not `None` by itself. Nested callee results/errors appear when they reach the seed's return. Callee panic or callback appears if it can occur in a traversed body, even when the seed never returns along that path. If a callee error is caught and handled, classify the seed disposition from the handler's outcome.

Starting at a procedure entry with symbolic formals, walk local branches, matches, short-circuits, returns and modeled loop summaries and substitute exact assignments, argument bindings and result projections. Follow outgoing direct calls, eligible closure and trait bodies, and exact field reads/writes on the supplied value or modeled local state. Stop at step depth and at the common boundaries. A callback case describes an invocation of a callable formal, or a source-proven local alias of it, with its exact actual port bindings regardless of whether its body is known. A requirement row is the failure alternative paired with its failing predicate. Assertions can expose a precondition this way. A comment or type signature alone is not a requirement.

Each `contract_case` row has:

- `procedure` (seed), `kind`, and `site` (the seed exit or reached panic/callback).
- `guards`, evaluated in source order under actual/formal bindings. Include preceding success and early-exit exclusions when required, even when opaque. Loops carry a typed recurrence and the branch taken on exit. Include relevant configuration atoms.
- `outcome` with return variant/constructor/field tree and source operands, error origin with written message/template/format operands if present, panic origin with assertion predicate and written payload, or callback formal identity with its ordered written actuals. A callee failure includes its propagation sites to the seed exit. Constructors and assignments each retain the original written operands.
- `dependencies`, the formal/receiver fields and opaque ports used by the outcome and its guards, with explicit reaching definitions/field projections, and `calls`, the crossed invocations and bindings relevant to the case. `recursive` or `opaque` references in the outcome carry the boundary span. Keep trace evidence sufficient to reconstruct the typed derivation, including selection and exclusion predicates. A reader can tell whether a proposed precondition is enforced from a closed guard dependency set.

Completeness means one case for each selected outcome site, each supported reaching guard combination and distinct resulting operand/port alternative. Coalesce loop iterations by source-backed recurrence, with the empty/exhausted path and explicit early exits if present. Unknown callback results do not make the observed invocation unknown. No claim of absent checks if a relevant frontier is open. For a return constructor built locally, an unknown leaf stays symbolic while certainty about its outer variant survives. Do not treat two statements from different invocations as one case.

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

## `value-transfers`

```text
(value-transfers :direction "upstream" :port "return" :depth 2
  (procedure-of (callable :name "target")))
```

`:direction` is `"upstream"` (default) or `"downstream"`. `:port` is `"return"` (default), `"argument"`, `"input"`, or `"write"`. These select, respectively, returned operands, written call actuals (receiver included), formal parameters (receiver included), or writes to lexical/field/index places in the seed body. A selector works in either direction. Selection includes exact modeled field/component subports of those operands. A seed's directly invoked closure has its own ports unless the selected operand in the seed is the closure's result or a captured place. `:depth` defaults to 2 and accepts 0–8. Eight covers the census's common wrapper and builder routes, including longer explicit proposals. The 12-edge configuration proposal needs persistence and worker transport beyond this source-local relation. Report depth distribution before freeze, including queries that remain open at the proposed boundary.

For every selected port with an incident transfer, trace backward to reaching definitions or forward to each consuming port. Consume a value at an evaluated read, argument, formal, branch test, returned component, assignment RHS/destination or known derived expression. Model transfers one edge at a time, including actual-to-formal, explicit/tail return to that exact caller's result, assignment-to-read, and projections through constructor operands. Keep call and return paired. At a seed return in downstream mode enumerate exact incoming call sites, and allow siblings entered through that caller if depth remains. At a seed formal in upstream mode enumerate exact supplying actuals. Else start with the body owning the selected port. For forward flow, emit every distinct use reached, including intermediate uses. For backward flow, retain all transfers in the trace to each maximal origin (literal, entry value, input field, unknown return, counted boundary or cyclic recurrence). Pure expression transformations keep ordered operands: a unary normalization, indexing or aggregate assembly is a dependency even when the output differs. Record which path component is preserved and which is transformed. A control dependency is recorded as a guard, not an ordinary value-transfer edge from a test into every expression it governs.

Follow exact fields of the same locally identified object through projections and nonescaping alias/borrow chains. Use local constructor-to-use paths and explicit field or modeled collection stores. Maintain written ordering and strong overwrite only when the same place is known; at a join retain alternative reaching stores with their predicates. Extend/append and iterator moves carry per-element identity only where the modeled operation determines it. Crossing closures, macros, traits and external bodies follows the shared rules. Global field name joins, lookup of a fresh object by a persistent key, protocol round trips, implicit reflection, concurrency, and arbitrary evaluation of inputs are out. A recursive fold can expose a syntactic constructor or recurrence under the call budget but cannot certify an unbounded whole-tree property.

Each `value_transfer` row has:

- `procedure`, `direction`, `port` and selected port identity. `source` and `sink` carry their containing callable, port tag, exact index/field path where known, site and written operand. The primary site is the sink. Split an aggregate into independently named fields; retain its whole written shape as an enclosing port.
- `transfers`, the ordered path from source to sink with edge tags (bind, store, read, actual/formal, return/result, construct, project, transform), sites and invocation bindings. Every step states what is transferred and the written operand or operator. For a shaped result, record the alternative's emitted constructor and field expressions at that endpoint, including sibling written operands and any proven local origin of each on that alternative. An opaque sibling remains opaque. Separate sibling transfer rows still carry each full trace.
- `guards`, the branch choices governing this route and its origin, scoped to the applicable versions of the bindings. A guard records selection, not a value taken from the predicate. `proof` distinguishes closed symbolic transfer from an established prefix meeting an open transition. An opaque call result is itself a valid source port. Its contents are unknown.

Completeness means all eligible incident transfers for every selected port and all guard-distinct routes, including branches, caller sites and aliases within the modeled domain and depth. Represent an applicable loop by its finite recurrence edges. A single hop (identity, edge tag, target candidate and invocation context) is the unit to enumerate and check, and to drop once for D if needed. Multiple displayed paths that use a shared hop must use the same hop result. A missing path is not proof of no write, no omitted element, or no transformation across an open alias or unmodeled body. Depth at an external/unknown origin is not credit for the claimed upstream value.

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

## `guarded-paths`

```text
(guarded-paths :direction "outgoing" :event "call" :depth 1
  (procedure-of (callable :name "target")))
(guarded-paths :event "call" :depth 0 (call :name "target"))
```

For a procedure seed, `:direction` is `"outgoing"` (default) or `"incoming"`. `:event` is `"call"`, `"write"`, `"return"`, `"raise"`, or `"all"` (default). A call includes method and callback invocations and visible command-builder calls. A write changes an already bound place, field or index. Local initializer declarations are reaching definitions rather than write events. A return includes explicit and tail exits and `?` exits. A raise is a written explicit panic or throw. A call-site input or structural call match selects only that exact written invocation as the endpoint, in its smallest enclosing executable procedure. Its only allowed selectors are outgoing and call (or all, which still means only the selected call); other combinations are query errors. `:depth` defaults to 1 and accepts 0–8. Zero covers local guards. Eight leaves room for the source-backed validator, error-handling and forwarding chains proposed by labellers. Protocol/scheduler order and generated dispatch need a different evidence model before an increase would help.

Outgoing procedures start at seed entry and follow source control to each selected local event or event in a bounded inlined body. Bind actuals, including returned and mutable-borrow state, where they are needed for guards or operands. Incoming procedures first enumerate the seed's exact caller chains up to depth, then walk from each selected ancestor's entry through the designated invocation, retaining the prefix and caller continuation. Emit prefix events along routes that reach the invocation and downstream events on its supported continuations. Include the seed body's events only along the designated invocation. No incoming route at depth zero. The anchor is the designated invocation, even if the row's primary event is an earlier or later caller operation. Charge each link of the ancestor-to-pivot chain against the depth bound. A sibling call used for evidence follows its own depth from the same execution root. A call-site event itself costs zero extra when its body is outside the route. For a call-site seed, follow the enclosing procedure up to that site; inlined calls before it can refine their success or operands, subject to depth. Record whether an invocation is synchronous or an awaited source-local future. A merely constructed future does not execute its body.

Evaluate local branch/match tests, early exits and source-backed loop recurrences. Track exact receiver fields and keyed read versions, same-local guard bindings, and call operands through supported reaching definitions. For calls whose guarded continuation is unknown, preserve a conditioned prefix and distinguish it from a guaranteed continuation. For a loop, state source order, per-iteration guards, and each source-proven continue/break/return/exhaustion relation. Earlier iterations' events are quantified in the order description, not multiplied into a list of sample iterations. For incoming lock context, state live exact local guards at the invocation. A scope exit or explicit drop ends liveness, and a move to an unknown owner opens it. For synchronous calls the held-guard relation can continue into the entered body. Do not infer a cross-thread order or an actual deadlock from an opaque mutex implementation.

Each `guarded_path` row has:

- `procedure` (seed), `entry` (execution root) and `pivot` (incoming invocation site if selected). `event` has a kind, exact source site (also the primary anchor), containing invocation and resolved target/place when known. Carry written actuals, assigned RHS, exit value, or error/panic origin according to the kind. Link each derived operand to its source and transformation. For command-building calls retain the written command and statically visible operand positions; do not infer shell expansion or execution.
- `guards` and `calls`, the path-specific condition sequence and entered invocations with actual/formal bindings. Guards identify the value version tested. Include exits avoided before the selected event. On a supported alternative that bypasses the event, the executed branch and early exit or continuation are retained on their own rows, rather than inventing a fictitious execution at the skipped event. An exclusion is warranted only when all routes under the proposed condition either bypass or are infeasible.
- `prior`, ordered earlier call, write, return or raise sites on this route, independent of the event selector. Record per-iteration order when a loop is involved. An earlier site on every feasible prefix under the stated guard is ordered on that guard. A site on only some prefixes is a conditional predecessor. Mutually exclusive outcomes do not create an order. `held` lists exact source-proven local guard acquisitions still live at the event, their protected place identities and the scope evidence. `continuation` records any proven local terminal/continue/break transfer at the event, or a return/await/error propagation edge when known. Do not mix a proof of order with a stronger statement that the later operation necessarily completes.

Completeness means all eligible event sites, distinct incoming pivot routes if selected, and all supported guard-distinct control prefixes for each, with ordered predecessor and live-guard evidence. Coalesce repeated loop prefixes by source-backed recurrence with all locally distinguishable branches. Emit all eligible siblings and exits under the selector. Incomplete dispatch, unmodeled effects on guards and unresolved continuation are open when they can change the answer. A source-local guard such as a false branch excluding a write in its body remains valid when that body contains an unknown call.

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
