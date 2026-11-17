# Preserve roles and show model costs within each agent group

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Usage by session must show top-level model costs within Primary agents,
Subagents, and Reviewers for All and individual profile selections. Terra
must use its published standard rates. Unknown costs must leave other model
totals visible, and all summary and session rows must remain reachable on a
small terminal.

## Progress

- [x] (2026-11-11) Investigated the live read-only report: Sol was recorded; unpriced Terra made the total unknown.
- [x] (2026-11-11) Committed Terra pricing and initial model summary as `53f044a8c`; CLI/TUI goldens and affected suites passed.
- [x] (2026-11-11) Reworked the summary into scrollable rows after review found fixed-header clipping; narrow-terminal golden passed.
- [x] (2026-11-11) User clarified that only Park/Close need role classification; full Destroy needs no backfill or schema change.
- [x] (2026-11-11) Added role/model aggregates from the daemon's existing filtered usage snapshot.
- [x] (2026-11-11) Rendered three role groups, plus unclassified fully destroyed children when present.
- [x] (2026-11-11) Store integration, HTTP compatibility and grouped TUI goldens pass, including navigation at 70x22.
- [x] (2026-11-11) Full suites for core, controller, TUI and CLI passed; workspace clippy, rustfmt and diff checks passed.
- [x] (2026-11-11) Implementation committed as `53f044a8c` and `3bf071766`. Final review/publication are tracked by Git review refs and origin/master.

## Surprises & Discoveries

`subagent_accounting` retains child ownership and task name after Destroy but
does not retain kind. `subagent_sessions.record_json` holds kind while the
operational relation exists. Already deleted roles cannot safely be inferred
from task names. Oracle-kind children belong with Subagents; Reviewer-kind
children belong with Reviewers, regardless of their parent's role.

## Decision Log

Use existing operational child records to classify Park/Close sessions. The
user explicitly excluded backfilling roles lost through full Destroy. Read
`subagent_sessions.record_json.kind` in the same transaction as retained
ownership and turns. No migration, backfill, or new persistent state is needed.
Retained usage without a child record is explicitly unclassified.

The daemon owns role aggregation because it reads filtered turns and durable
ownership in one database snapshot. Add a defaulted role summary field to the
report response. Keep existing per-session model totals and overall unknown
cost semantics unchanged. The terminal renders role summaries in the same
scrollable list as session details, with stable selection identities.

## Context and Orientation

`mj-core/src/usage/pricing.rs` stores base token rates. Shared response types
live in `mj-core/src/usage/report.rs`. `mj-controller/src/database/usage_report.rs`
reads one SQLite transaction, filters turns by recorded profile and time,
and includes retained descendants. `mj-controller/src/database/schema.rs`
contains forward migrations; `mj-controller/src/database.rs` declares the
current revision. Child registration inserts `subagent_sessions` rows and
cleanup removes them; retained `session_contexts` and accounting survive.
`mj-tui/src/session_usage_report.rs` renders the report, and the existing
`golden_dashboard_dialogs` test covers the whole rendered surface.

## Plan of Work

Next add role summary types and a defaulted response field. Classify top-level
members as Primary, known Worker/Oracle children as Subagents, Reviewer children
as Reviewers, and children without retained kind as unclassified. Merge each
already-priced contribution into its session model totals and its role/model
totals without counting a turn twice. Apply existing filters before aggregation.

Finally render role headers, their costs, and model rows before session rows.
All rows participate in the existing scroll list. Extend the store integration
case with Park/Close, full child deletion and profile filtering, and extend the
existing TUI golden with role groups and narrow navigation.

## Concrete Steps

Run commands from the repository root, using normal mbx Cargo storage and
elevated execution for every cargo test. Focused validation uses:

    MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-core -p brokk-mjolnir -p brokk-mj-tui -p brokk-mj-controller golden_dashboard_dialogs
    cargo test -p brokk-mj-core -p brokk-mjolnir -p brokk-mj-tui -p brokk-mj-controller usage_report

After the last edits run full touched-crate suites once, workspace
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`,
and `git diff --check`. Tests retain their existing isolated temporary stores
and named instances. Never run the new binary against live/default data.

## Validation and Acceptance

The golden must display separate Primary agents, Subagents and Reviewers
headers with model amounts, including Sol/Terra and an unknown model. All and
Codex views must have different filtered amounts. Narrow-terminal navigation
must reach the last summary model and last session. Store tests must prove
reviewer identity remains available after Park/Close and role/model amounts
equal the filtered turn contributions. Full Destroy must leave retained usage
unclassified instead of losing or misclassifying its cost.

## Idempotence and Recovery

The report uses one read-only transaction and introduces no schema changes.
Do not install without an installation request. Commit only task-owned files on the current
branch; merge upstream if needed and push after required review.

## Artifacts and Notes

Issue: #1501. First validation logs are in
`/mnt/optane/tmp/mj-terra-1501-tests.log` and
`/mnt/optane/tmp/mj-terra-1501-clippy.log`.

## Interfaces and Dependencies

Use existing rusqlite, serde and cost helpers. Add `UsageAgentRole`,
`AgentUsageEstimate`, and a defaulted `SessionUsageReport.agent_usage` field.
No crate, protocol revision, new runtime task, or external service is needed.

## Outcomes & Retrospective

Initial Terra pricing and ungrouped model totals are validated. Role grouping
and Park/Close classification pass focused store/API and rendered golden tests.
The full touched-crate suites, workspace clippy and formatting checks pass.
The initial review's clipping finding is fixed with scrollable role/model rows.
Two later review preparations timed out; a third reported a closed node stream.
The installed daemon still reports the session running; retry review after that
connection check, then merge unrelated upstream work and push. No installation
or live store change is part of this request.

Revision note: expanded the initial pricing fix to the explicitly requested
role/model aggregation, and retained the review-driven scrolling correction.

Revision note: the user excluded backfilling full Destroy. Simplified role
classification to existing child records and removed the planned migration.

Revision note: recorded full validation and review transport failures. The
implementation is complete; Git review refs and upstream history record delivery.
