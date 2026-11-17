# Scope of the requested-SID usage cost

The checked-out implementation sums **only the requested SID's priced whole-turn usage** for `mj usage --session SID --json`. It does not traverse reviewer/child sessions on that path. The actual serialized field is `turn_cost_usd_ticks` (10^-10 USD), not `total_cost_usd_ticks`. Parent/child aggregation is a separate explicit API/CLI path.

Inspected checkout HEAD: `df5d052269dd390d76dc91b1197a96931817aacc`. All references below refer to that checkout.

## Actual call chain and sum ownership

- `mj-cli/src/api_commands.rs:460`, symbol `usage`: `--session` calls `client.usage(&session, after_seq, limit)` at472, then prints the returned page unchanged as JSON at474. `--parent` instead calls `usage_tree` at463. They are separate argument branches.
- `mj-cli/src/api_client.rs:607`, `ApiClient::usage`: requests `/sessions/{session_id}/usage?after_seq=...&limit=...` at613–617. `ApiClient::usage_tree` at621 requests `/sessions/{parent}/usage/tree`.
- `mj-controller/src/server/api/turns.rs:354`, `usage`: forwards the path SID to the backend `.usage(session_id, after_seq, limit)` at360–365 and returns its `UsagePage` at367.
- `mj-controller/src/server/api/subagent_backend.rs:379`, backend `usage`: forwards that SID directly to `database::load_session_usage(&session_id, after_seq, limit)` at387. It invokes no usage-tree/child traversal.
- `mj-controller/src/database/usage.rs:56`, `load_session_usage`, forwards to `load_session_usage_from` at62. The latter at72 queries page turns using `WHERE session_id=?1` at88. Crucially, pagination affects only the returned `turns`: at103 it separately calls `session_summary(&tx, session_id)` without page limits, and at114 copies `summary.turn_cost_usd_ticks` into the root page.
- **Decisive filter:** `mj-controller/src/database/usage.rs:257`, `session_summary`. Its SQL at260–261 selects `session_turn_usage u LEFT JOIN session_turn_selections s USING(session_id, command_id) WHERE u.session_id=?1`; at262 it binds `[session]`. The loop at271 reads only those rows. No join to `subagent_accounting`, recursive query, parent lookup, or child summary occurs here.
- **Decisive dollar calculation:** same symbol at276–317. Only `UsageScope::Turn` rows enter the priced branch. A nonpartial provider turn cost is used at279–282/301–303; otherwise the selected/attributed model counters of that same row are estimated at283–297/304–306. At311–316, `summary.turn_cost_usd_ticks.checked_add(cost)` adds that row's supported cost. Missing-cost/non-turn reports add coverage gaps, not guessed dollars. `provider_session_cost` is loaded separately at335–340 and is not added to `turn_cost_usd_ticks`.
- `mj-core/src/storage.rs:412`, `UsagePage`: field documentation at422–424 defines the root scalar as the sum of priced whole turns, partial when cost coverage is missing.

## Where children would be included

`mj-controller/src/database/usage.rs:348`, `load_usage_tree_from`, uses the explicit recursive `subagent_accounting` query at355–358. It obtains each member SID's `session_summary` and merges them at372–373. `merge_summary` adds member costs at420–425. That is reached by the separate `--parent`/`usage_tree` path, not the recorded `--session` calls.

Thus, for this implementation, adding separate carrier and reviewer **requested-SID root** cost scalars does not double-count reviewer/child sessions. The root summary is global across the requested SID's own turns and repeats on each pagination page; one root perSID must be counted, as the ledger does. This source establishes the ownership directly, beyond the earlier token-model corroboration. It does not resolve missing/unfinished usage, and all incomplete totals stay null.

## Deployment identity limit

Already captured operator evidence only:

- `arms/ordinary-review-driver/cli-version.txt`: `mj 2.71.0`.
- `arms/ordinary-review-driver/cli-api-info.txt`: API1 at the recorded production endpoint; no daemon/build commit.
- `arms/ordinary-review-driver/invocation.json`: launch checkout `60b1ac91d3453676709ad6e90e6616d3135d41a2`; `runtime_commit=be74a5cb9` identifies the review harness files, not the installed mj CLI/daemon executable.
- Current `Cargo.toml:10` also says version2.71.0. A matching version string is not proof of an identical binary/daemon source revision.

The captured files do **not** tie the campaign's running daemon implementation to current HEAD. Therefore the decisive conclusion above is checkout implementation behavior, not independently established deployment equivalence. No live CLI/network/service inspection, version archaeology, current verifier data, code edits, builds or tests were performed. Only this new note was added; frozen manifests and existing ledger artifacts remain untouched.
