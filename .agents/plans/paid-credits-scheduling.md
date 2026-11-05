# Share paid-credit scheduling across subagents and quota recovery

This ExecPlan follows `.agents/PLANS.md` and is a living document.

## Purpose / Big Picture

When Claude Code or Codex profiles exhaust their included subscription quota,
Mjolnir should continue assigning work to accounts with paid extra-usage funds.
Subagent launches and quota recovery must use the same weighted selection:
included quota first, then usable paid balances, with more funds receiving more
work. The user explicitly clarified that credits means paid extra usage, not
the banked reset credits already displayed in the quota pane.

## Progress

- [x] (2026-11-05) Trace selection, provider reports, recovery, and provider schemas.
- [x] (2026-11-05) Collect paid balances and preserve them through quota publication and caching.
- [x] (2026-11-05) Extract one scheduler and use its weighted ordering for both paths.
- [x] (2026-11-05) Extend provider, MCP spawn, cache, model-scope, recovery, and weighted-distribution regressions; focused paid tests pass.
- [x] (2026-11-05) Complete the full client/controller/TUI suites and workspace Clippy; both isolated controller reruns pass.
- [ ] Commit, obtain required review, and push the current branch to origin/master.

## Surprises & Discoveries

Quota recovery calls the subagent ranker but then independently excludes every
exhausted subscription and always chooses the first ranked account. Subagents
instead draw a weighted random order. The daemon currently discards all paid
credit fields. Claude's usage request explicitly sets `skip_spend=1`.

The installed Codex app-server schema describes `credits.hasCredits`,
`credits.unlimited`, and a nullable string `credits.balance`. Claude's installed
CLI reads extra-usage enabled status and monthly spending from `/api/oauth/usage`
and prepaid funds from `/api/oauth/organizations/:orgUUID/prepaid/credits`.
Its OAuth profile response supplies `organization.uuid`. Wallet amounts and
spending caps use minor currency units. Monthly headroom alone is not funds.

The old quota score checked zero remaining before checking whether a reset had
passed. Reordering those checks keeps expired zero readings from unnecessarily
choosing paid usage. Claude's macOS CLI quota path does not expose structured
wallet data, so paid funds remain unknown on that existing path.

## Decision Log

- Decision: Keep subscription quota ahead of paid funds, and preserve health blocks.
  Rationale: Paid usage should follow included usage; funds cannot override an actual provider failure.
  Date/Author: 2026-11-05, Codex.
- Decision: Use the lesser of Claude's prepaid wallet and remaining monthly allowance.
  Rationale: Either can stop paid inference. An unavailable wallet is unknown rather than invented spending capacity.
  Date/Author: 2026-11-05, Codex.
- Decision: Preserve paid-credit information as an optional, backward-compatible quota-report field.
  Rationale: Existing stored reports and older clients must continue to decode; there is no SQL schema change.
  Date/Author: 2026-11-05, Codex.
- Decision: Draw separately within each capacity tier, keeping unknown subscription data before paid usage and paid balances before unmetered API accounts. Explicit unlimited funds or known funds of undisclosed amount receive unit weight.
  Rationale: Relative finite balances must never let a large wallet displace included quota. Existing unknown-quota preference is preserved.
  Date/Author: 2026-11-05, Codex.

## Outcomes & Retrospective

Paid balance collection and the shared scheduler are implemented. Focused
Claude HTTP and deterministic distribution tests passed (2 tests). Client tests
passed (44 module tests and one process integration test). The controller suite
passed 2,272 tests, with 19 ignored; both remaining tests passed when rerun
individually. Those were a login-shell startup timeout under parallel load and
a Settings golden whose default port was changed by the supplied MJ_INSTANCE.
The full TUI suite and workspace Clippy pass. Existing provider failure marks
and the long-window threshold for automatic session moves remain unchanged.
The implementation is ready for commit and required pre-push review.

## Context and Orientation

`mj-client/src/quota.rs` defines `ProfileQuota`, the serialized report shared
between the daemon and its clients. `mj-controller/src/claude_usage.rs` and
`codex_usage.rs` query providers; `mj-controller/src/quota.rs` projects their
answers into these reports. `server_runtime/api.rs` converts reports into
candidate profiles. `server/api/subagents.rs` currently owns both ranking and
weighted random ordering. `daemon/continuation/quota.rs` chooses a destination
when an interrupted session's long subscription allowance cannot reset soon.
Provider health marks separately block accounts whose requests have failed.

## Plan of Work

Add a serializable paid-credit value that distinguishes a finite balance,
known available funds of unknown amount, and explicitly unlimited funds.
Do not conflate any of these with banked reset counts. Extend provider parsers
and quota projection, retaining fresh values rather than merging stale funds.
Claude's optional wallet read is bounded and asynchronous; failure is logged
and leaves paid capacity unknown without destroying valid subscription data.

Move candidate ranking and weighted ordering into
`mj-controller/src/profile_scheduler.rs`. Keep model-scoped subscription
windows, unknown quota, usage-billed API profiles, and health marks working.
Order by capacity tier before drawing within a tier, so arbitrarily large
paid balances cannot outrank included quota. Draw finite paid balances in
proportion to their remaining funds. Quota-recovery destinations must ask this
same scheduler whether capacity is exhausted and use its weighted draw.

## Concrete Steps

Run commands from the repository root. Keep the existing mbx Cargo wrapper and
build storage unchanged. All `cargo test` commands require elevated execution.
Use `--instance paid-credits-scheduling` for any daemon/CLI invocations. Unit
tests already isolate their stores with MJ_CONFIG_DIR and MJ_DATA_DIR; run them
with `env -u MJ_INSTANCE` to preserve default-port golden text. First run focused
controller tests for provider reports, profile selection, and quota moves.
After final edits run `cargo test -p brokk-mj-client`,
`cargo test -p brokk-mj-controller`, `cargo test -p brokk-mj-tui`, and
`cargo clippy --all-targets -- -D warnings`. Format with `cargo fmt --all`.

## Validation and Acceptance

Provider integration cases should prove that Codex's bucket-specific credits
and Claude's enabled extra usage, wallet, and monthly cap survive publication.
Extend the deterministic weighted-draw regression to prove paid accounts receive
work proportional to balances only after included quota is depleted. Exercise
quota-move filtering with funded exhausted accounts, zero funds, disabled extra
usage, incompatible harnesses/models, and unhealthy profiles. Old serialized
reports must still decode with no paid balance. Subscription reset timing and
in-flight continuation admission must keep passing existing regressions.

## Idempotence and Recovery

No live daemon, credentials, billing settings, or prepaid balances are changed.
Provider calls only read usage. Repeated tests use their own directories and
instances. No database migration or harness upgrade is needed. If upstream
advances before push, fetch and merge it into the current branch, retaining
unrelated changes and resolving conflicts before requesting review again.

## Artifacts and Notes

The local schema generated with `codex app-server generate-json-schema --out
/tmp/mj-paid-credits-schema` confirms the paid-credit object independently of
reset-credit metadata. Official product behavior was checked at
`https://learn.chatgpt.com/docs/app-server` and
`https://support.claude.com/en/articles/12429409-manage-usage-credits-for-paid-claude-plans`.

## Interfaces and Dependencies

Use existing serde_json numbers for finite amounts so quota reports retain Eq
and introduce no dependency for monetary accounting. Scheduling uses positive
finite floats only as relative weights, never for billing. The shared
scheduler exposes ranking, weighted ordering, and capacity classification;
API failures wrap random-source errors using the existing anyhow conversion.

Revision 2026-11-05: Initial plan, incorporating the user's paid-usage clarification.
Revision 2026-11-05: Record implemented collection, shared scheduling, reset-clock fix, focused evidence, and the existing macOS reporting limitation.
Revision 2026-11-05: Record full-suite evidence and correct the unit-test command environment: their own isolated stores are sufficient, and a global named instance changes golden port text.
Revision 2026-11-05: Record successful isolated reruns, full TUI validation, and workspace Clippy.
