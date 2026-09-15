# Turn review: what was ported from mjolnir, and what was not

Hel's turn review (`src/hel_review/`, `src/hel_chat/turn_review.rs`) is a port of
mjolnir's discrete review, whose source is `mj-agents/src/discrete_review.rs`
with its contract types in `mj-core/src/orchestrator_contract.rs`. The two
repositories may re-merge, so this file records exactly what crossed over, what
was deliberately left behind, and where the wording had to change. Line numbers
are from the mjolnir checkout as of 2026-08-31; treat them as a starting point,
not a guarantee.

## What was ported, and where it lives now

The lane roster, the shared prompt fragments and every prompt builder are in
`src/hel_review/lanes.rs`. That covers `REVIEW_LANES` and `QUICK_LANE`
(`:155-257`, each lane keeping its `bifrost_tools`), the fragments at `:96-121`
(`INTENT_PREAMBLE`, `REVIEWER_PREAMBLE`, `SUPERVISOR_PREAMBLE`,
`VALIDATOR_PREAMBLE`, `DIRECT_INTENT_CONTEXT`, `QUICK_INTENT_CONTEXT`,
`REVIEW_ORACLE`, `QUALIFICATION_GATES`, `PRIORITY_FINDING_CONTRACT`), the quick
tier's prompts (`:1679-1755`), the supervisor prompt and its pass context
(`:2027-2140`), the lane prompt and lane context (`:2760-2913`), the intent
analyst (`:2819-2837`), `user_messages_packet`, `should_extract_intent`, the
roster text, and the dispatch argument types and validation (`:292-376`).

Verdict classification is `src/hel_review/verdict.rs`: `synthesis_verdict`
(`:2721-2747`), `lane_report_is_clean` (`:1664`), both sentinels, and the
contract types from `orchestrator_contract.rs:285-500` (`ReviewVerdict`,
`ReviewPassEvidence`, `ReviewLaneEvidence`). The size-bounding helpers
(`bound_review_section`, `bound_tail`) are in `src/hel_review.rs`.

Bifrost's `analyze_diff` decoding, its changed-callable packet formatting
(`:2142-2358`, `:2618-2657`) and the analysis invocation are in
`src/hel_review/bifrost.rs`. `RawDiffSummary` (`:2159-2223`) is in
`src/hel_review/delta.rs`.

The tier structure and the supervisor loop are re-expressed in
`src/hel_review/driver.rs`: the quick tier's validator-skip on a clean report
(`run_quick`, `:1358-1545`), and the extended tier's rule that a verdict is
accepted only when no launched lane is outstanding and the report queue is
drained (`drive_supervisor`, `:1825-1972`), including mj's two injection
instruction variants.

The ported unit tests came with the code they cover: verdict classification,
`RawDiffSummary`, prompt rendering, dispatch validation, and intent-analyst
gating.

## Where the review runs (converged with mj, 2026-09-01)

mj hosts its review driver in the process that owns the session, for every one
of its surfaces (`mj-core/src/orchestrator.rs`, spawned by the TUI, by headless,
and by its web host). Hel first hosted the driver in the terminal's view object,
which is why a session driven from a phone was never reviewed and a killed
terminal could strand a session's prompts. Hel now matches mj's shape: the
driver lives in the controller daemon (`src/hel_review/host.rs`), and the
terminal and the phone are both projections that render a published view and
send resolutions back.

Two of mj's hosting rules were ported with it, and are cited at their code:

* Every asynchronous result carries the epoch of the review that asked for it,
  and results naming another epoch are dropped -- mj's `review_outcome_rx` arm.
  Without it a cancelled review's late capture lands on its successor.
* A second start is refused while one is already being prepared -- mj's
  `discrete_review_started`.

Arming converged too: mj arms review from its global config file, and Hel now
arms it from `[review]` in `config.toml` rather than from a per-workspace row
written by a UI.

What still differs deliberately: mj's verdict can start an automatic correction
round above a configured threshold, and it holds the completed turn with
`held_completion`. Hel shows the findings to the person and lets them choose,
and holds the turn by refusing prompts for that session in the daemon's submit
path.

## What was deliberately not ported

* mj's host-side spawn kernel -- `ProgrammaticPool`, seats, roster, quota
  failover -- because Hel already runs reviewing agents as sidecars inside the
  session's worker container, with the profile staging, relay transport and pane
  it built for plan review.
* The workflow-graph events (`WorkflowEmitter`, `WorkflowTransition`,
  `ActorWaiting`) that drive mj's own UI. Hel's pane reads driver state.
* `correction_threshold` and `max_correction_rounds`. mj needs them because its
  correction loop is autonomous; Hel shows the findings to the user, who decides
  whether to forward them, so the knobs are user actions instead of settings.
* mj's per-session runtime override of the global arming (its `review_enabled`
  atomic). Hel has global arming and a one-off `/review`; a per-session override
  is noted and not built.
* `held_completion` and the rest of mj's turn-holding machinery. Hel holds the
  turn by holding the composer: the review view owns the screen, the prompt
  submission paths refuse, and the web surface refuses a prompt whose session has
  an in-flight review.
* mj's `bifrost_analysis` enable flag and its raw-diff prompt branch
  (`:1013-1043`). Hel has one path: Bifrost is required, and its failure fails
  the review with a message naming the image rebuild as the fix.
* npm-based Bifrost. mj runs `npx @brokkai/bifrost` because it has no checkout;
  Hel bakes a pinned crates.io release (`brokk-bifrost`) into
  `containers/Containerfile.agent-dev`, so a review fetches nothing.
* mj's loopback-TCP `BridgeServer` for the dispatch tool. Hel's supervisor tool
  talks to its worker over a Unix socket inside the worker root
  (`src/hel_review/mcp.rs`, served in `src/hel_worker_runtime/unix.rs`).
* `MAX_PARALLEL_LANES` was lowered from six to three: Hel's lanes run inside one
  resource-limited container beside the primary agent.

## Where the wording had to change

Three kinds of change were unavoidable, and each is marked at its site:

1. The product name in sentences that tell a model how the user can stop it.
   "The user can cancel it manually through Mjolnir's visible Stop action"
   became "The user can cancel it at any time from Hel's review pane."
2. The dispatch tool's name: both register the server as `mj-review`, but
   Hel's tool is `spawn_specialist` where mj's is `call_review_subagents`;
   each prompt names the tool its supervisor actually has.
3. `PRIORITY_FINDING_CONTRACT`. mj's sentence names its configured automatic
   correction threshold, which Hel does not have. Hel's says the user reads the
   surviving findings and decides whether to send them back.

Two smaller divergences: `QUICK_INTENT_CONTEXT` and `intent_prompt` drop mj's
sentences about `steered_mid_turn` messages. Hel does steer prompts into a
running turn, but the projection records a steered prompt as an ordinary user
transcript item (`src/hel_projection.rs`, `RelayCommandOutcome::Steered`), so a
review reads it in chronological order and cannot mark it. Telling a model to
look for a mark that never appears would be worse than leaving it out. Second,
`RawDiffSummary::diffstat` drops mj's "(raw Git patch; Bifrost analysis
disabled)" suffix, because in Hel that summary is the worker's ordinary
diffstat rather than a fallback.

## What Hel added that mj has no equivalent for

* Cumulative capture from a per-repository Git tree baseline that advances only
  when a review resolves (`src/hel_archive/git.rs`, `src/hel_review/delta.rs`).
  mj reviews a turn's own snapshot; Hel folds a cancelled review's turn into the
  next one.
* A baseline whose tree object the repository no longer holds -- after a resume
  onto a fresh target -- restarts coverage instead of presenting the whole
  repository as the turn's work.
* Per-harness MCP delivery: harnesses that accept a server over ACP get it
  there, while Claude and Kimi read servers from their staged profile, which the
  controller patches while staging the reviewer
  (`src/hel_controller/reviewer.rs`).
