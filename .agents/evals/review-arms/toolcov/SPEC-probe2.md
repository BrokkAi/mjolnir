# Tool coverage, probe 2: query steps and flow policies

Probe 1 (`toolcov/SPEC.md`, `results/tool-coverage.jsonl`) tried only
Bifrost's navigation tools (`analyze_diff`, usage scans, `usage_graph`,
`get_definitions_by_location`, `query_code` text search). Its A0 and B classes
therefore mean "not returned by those tools", not "Bifrost cannot". Probe 2
asks the same records' questions with the query steps and the flow engine, as
the Bifrost CPG session advised, so that tickets are filed only for what
still fails.

## Inputs

- Your record: the census `fact` and probe 1's record in
  `results/tool-coverage.jsonl` (same directory as this file's parent), for
  the decisive anchor and the diff-only question.
- A worktree at the record's introducing commit:
  `/mnt/optane/tmp/toolcov2/<id>`. Base is `HEAD^`. Do not modify tracked
  files; you may add `.rqlp` policy files under `.bifrost-probe/` in the
  worktree.
- Bifrost 0.15.2 at `bifrost`. Run a tool with
  `bifrost --root <worktree> --tool <name> --args '<json>'`.
  `bifrost --help query_code` documents every step's typed signature;
  `bifrost --help run_policy` documents policy runs. References:
  https://bifrost.brokk.ai/reference/code-query/ ,
  https://bifrost.brokk.ai/reference/data-flow-and-typestate/ ,
  https://bifrost.brokk.ai/reference/static-analysis-policies/ .
- Run one Bifrost process at a time; never several in parallel (all worktrees
  share one cache, and concurrent processes block on its build lock).

## What to try, by record

- `catalog-cache-key-collision` (A1): `query_code` with the `call_input` step
  to get the argument bound to the `model` parameter at each call site of
  `load_profile_config_cache` and `save_profile_config_cache`.
- `metadata-checkpoint-overwrites-checkout` and
  `unused-thread-above-archive-floor` (A3): `state_events_of` for the field
  (`review_refs_metadata_only`; `recovery_floor_ordinal`), to get its
  establish, kill and read events with the assigned values.
- `restore-through-profile-symlink` (A4): `guards_of` and
  `control_relations` for the call to `clear_relay_state_plan`, to get the
  match arm (`WorkerRootReset::FreshTarget`) it runs under.
- `codex-effort-alias-lost` (B3), `export-relative-read-root` (B4),
  `symbol-change-keeps-cached-transcript` (B6), and the last hop of
  `github-token-in-container-argv`: a `.rqlp` policy with `:type flow`, run
  through `run_policy`, with sources and sinks chosen from what the diff
  shows (for example, source: the value the diff injects; sink: the call or
  field the fact names). This is the only released surface that runs the
  interprocedural engine.
- `held-prompt-ignores-interrupt` and `journal-valid-json-corruption` (A6):
  a flow or reachability query asking whether any path connects A to B
  (the interrupt route to the held prompt's wait loop; the journal reader to
  `validate_relay_event`), and whether the answer is stated as complete.

Pick sources, sinks and selectors only from what the diff shows; do not name
the decisive anchor directly unless it is in the diff.

## Record for each attempt

The step or policy (with its full arguments or `.rqlp` text), the outcome
(`works`: the output states the decisive relation; `incomplete`: it returns
something but reports incompleteness, with the diagnostic; `unsupported`: the
step or flow is not available for Rust, with the message; `wrong`: it
answers but not with the fact), the result count and rank of the decisive
item, completeness status, wall time, and errors. Cap each record at 30
minutes of tool time. A failure is a result; do not work around it.

Return one object per record through the StructuredOutput tool with `id`,
`attempts` (each: `surface`, `input`, `outcome`, `result_count`, `rank`,
`status`, `seconds`, `diagnostic`), `best_outcome` (works, incomplete,
unsupported, wrong), `capability` (A1, A3, A4, A6, B3, B4, B6 or github-hop),
and `note` (one or two sentences: what a ticket would need to say). Use plain,
literal language.
