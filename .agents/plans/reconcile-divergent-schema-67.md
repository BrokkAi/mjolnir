# Preserve both database histories when merging master

This plan follows `.agents/PLANS.md` and records the merge of local commit
8da59e6d with incoming d9953118. The user requests conflict resolution and push,
with targeted tests only.

## Purpose and context

Both branches assigned database revision 67 to different breaking changes.
Local 67 makes usage accounting durable and 68 adds failed-startup cleanup;
incoming 67 introduces project aliases, accepted snapshots, and discovery.
`mj-controller/src/database/schema.rs` runs migrations, while `database.rs`
sets the supported revision. Resolving the textual conflict alone would leave
one branch's existing stores missing tables. Neither older writer supports
the combined schema. No live store is used during validation.

## Progress

- [x] Inspected all five conflicts and both migration histories.
- [x] Preserved local 67/68 and added breaking reconciliation revision 69.
- [x] Added isolated regressions for both 67 histories, local 68, and interruption.
- [x] Four divergent-history migration regressions passed; format and diff checks passed.
- [ ] Finish existing focused regressions and lint checks.
- [ ] Commit the existing merge on master and push to origin/master.

## Decision Log

Keep the original local 67/68 transaction boundaries. At 69, identify the
accounting branch by its transactionally created subagent_accounting table,
apply the missing accounting changes if needed, and install the original
project schema with its existing idempotent SQL. Advance revision and minimum
compatible revision to 69 in the same transaction. This supports incoming 67,
local 67/68, and an incoming upgrade interrupted after 68. Revision 68 already
rebuilds sessions from its stored definition and restores indexes/triggers,
so it preserves incoming project columns and discovery triggers.

Use protocol 49 to distinguish the combined startup cleanup and project
protocol from either branch. Extend the frozen management transcript coverage.
Retain incoming crate-wide worktree helper visibility and the local lifecycle
owner's bounded cleanup budget; remove the obsolete cancellation-executor
import. Retain both branches' historical test cleanup.

## Milestones and concrete steps

From `/home/jonathan/Projects/mjolnir`, resolve the five conflicted files,
update `mj-controller/src/daemon/tests.rs` for protocol 49, and freeze incoming
revision 67 SQL in `mj-controller/src/database/project_catalog_v67.sql` for
regression inputs. The four `divergent_` tests in schema.rs create temporary
stores and verify accounting retention, project snapshots/aliases, discovery
triggers, startup cleanup, old-writer refusal, and interrupted migration retry.

Run `cargo test -p mj-controller divergent_ --lib` outside the restricted
sandbox, then targeted existing accounting, project, daemon management, and
Jev tests. Run formatting and lint checks. Stage explicit resolutions while
retaining the user's already staged clean merge changes, commit the merge,
and push normally to the configured upstream. Never force push or rebase.

## Validation and acceptance

All four divergent-history regressions must pass. Existing project and usage
behavior tests must pass against the combined schema. Management transcripts
must preserve compatibility with prior daemons. Targeted Jev tests must retain
the probability gates from the local branch. Git must report no unresolved
paths, the merge commit must have both original parents, and origin/master
must contain it after the push.

## Idempotence and recovery

The reconciliation transaction rolls back on failure and retries on restart.
Tests cover interruption before revision 69 is recorded. Test stores use
separate temporary directories; no daemon or host store is modified. Git's
existing merge state is retained until validation completes.

## Surprises & Discoveries

Revision numbers alone cannot identify which feature set a revision-67 store
has. The feature-specific schema distinguishes the two atomic histories.

## Outcomes & Retrospective

The four divergent-history regressions passed (4 passed, 0 failed; 2,076
filtered out). They executed in 1.80 seconds after compilation. Formatting
and staged whitespace checks passed. Existing focused regressions and lint
checks remain in progress.
